//! `std::process` через capability process ABI RustOS.

use super::env::{CommandEnv, CommandEnvs, CommandResolvedEnvs};
pub use crate::ffi::OsString as EnvKey;
use crate::ffi::{OsStr, OsString};
use crate::num::NonZero;
use crate::path::{Path, PathBuf};
use crate::process::StdioPipes;
use crate::sys::fs::File;
use crate::sys::pipe::{self, Pipe};
use crate::{fmt, io, ptr, thread};

const PROCESS_ABI_VERSION: u32 = 2;
const PRIORITY_INTERACTIVE: u8 = 3;
const MAX_CAPABILITIES: usize = 8;
const MAX_TABLE_BYTES: usize = 2048;

const ROLE_EXECUTABLE_NAMESPACE: u16 = 1;
const ROLE_STDIN: u16 = 4;
const ROLE_STDOUT: u16 = 5;
const ROLE_STDERR: u16 = 6;

const RIGHT_TRANSFER: u64 = 1 << 9;
const STATUS_OK: i64 = 0;
const STATUS_BUSY: i64 = -11;

#[repr(C)]
#[derive(Clone, Copy)]
struct SpawnCapability {
    source: u32,
    target_slot: u16,
    role: u16,
    rights: u64,
}

#[repr(C)]
struct ProcessSpawnRequest {
    version: u32,
    flags: u32,
    path_address: u64,
    path_length: u32,
    priority: u8,
    reserved0: [u8; 3],
    arguments_address: u64,
    arguments_length: u32,
    argument_count: u32,
    environment_address: u64,
    environment_length: u32,
    environment_count: u32,
    capabilities_address: u64,
    capability_count: u32,
    namespace: u32,
}

#[repr(C)]
struct ProcessSpawnResult {
    process: u32,
    reserved: u32,
    pid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ExitReason {
    status: i32,
    exception: u16,
    flags: u16,
    fault_address: u64,
}

pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    env: CommandEnv,
    cwd: Option<OsString>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

#[derive(Debug)]
pub enum Stdio {
    Inherit,
    Null,
    MakePipe,
    ParentStdout,
    ParentStderr,
    Pipe(Pipe),
    InheritFile(File),
}

#[derive(Clone, Copy)]
enum DefaultStdio {
    Inherit,
    Null,
    MakePipe,
}

impl Command {
    pub fn new(program: &OsStr) -> Command {
        Command {
            program: program.to_owned(),
            args: vec![program.to_owned()],
            env: Default::default(),
            cwd: None,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn arg(&mut self, argument: &OsStr) {
        self.args.push(argument.to_owned());
    }

    pub fn env_mut(&mut self) -> &mut CommandEnv {
        &mut self.env
    }

    pub fn cwd(&mut self, directory: &OsStr) {
        self.cwd = Some(directory.to_owned());
    }

    pub fn stdin(&mut self, stdin: Stdio) {
        self.stdin = Some(stdin);
    }
    pub fn stdout(&mut self, stdout: Stdio) {
        self.stdout = Some(stdout);
    }
    pub fn stderr(&mut self, stderr: Stdio) {
        self.stderr = Some(stderr);
    }
    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        let mut iter = self.args.iter();
        iter.next();
        CommandArgs { iter }
    }

    pub fn get_envs(&self) -> CommandEnvs<'_> {
        self.env.iter()
    }
    pub fn get_env_clear(&self) -> bool {
        self.env.does_clear()
    }
    pub fn get_resolved_envs(&self) -> CommandResolvedEnvs {
        CommandResolvedEnvs::new(self.env.capture())
    }
    pub fn get_current_dir(&self) -> Option<&Path> {
        self.cwd.as_ref().map(Path::new)
    }

    pub fn spawn(
        &mut self,
        default: Stdio,
        needs_stdin: bool,
    ) -> io::Result<(Process, StdioPipes)> {
        let default = default_kind(&default)?;
        let stdin = resolve_stdio(
            self.stdin.take(),
            if needs_stdin {
                default
            } else {
                DefaultStdio::Null
            },
        );
        let stdout = resolve_stdio(self.stdout.take(), default);
        let stderr = resolve_stdio(self.stderr.take(), default);

        let mut parent_pipes = StdioPipes {
            stdin: None,
            stdout: None,
            stderr: None,
        };
        let mut child_pipes = Vec::new();
        let mut extra = Vec::new();
        prepare_stream(
            stdin,
            ROLE_STDIN,
            true,
            &mut parent_pipes.stdin,
            &mut child_pipes,
            &mut extra,
        )?;
        prepare_stream(
            stdout,
            ROLE_STDOUT,
            false,
            &mut parent_pipes.stdout,
            &mut child_pipes,
            &mut extra,
        )?;
        prepare_stream(
            stderr,
            ROLE_STDERR,
            false,
            &mut parent_pipes.stderr,
            &mut child_pipes,
            &mut extra,
        )?;

        let (path, execution_args) =
            executable_plan(&self.program, &self.args, self.cwd.as_deref())?;
        let arguments = encode_strings(execution_args.iter().map(OsString::as_os_str))?;
        let mut environment_map = self.env.capture();
        if let Some(cwd) = &self.cwd {
            let absolute = crate::sys::paths::rustos::absolute(Path::new(cwd))?;
            environment_map.insert(OsString::from("PWD"), absolute.into_os_string());
        }
        let environment_count = environment_map.len() as u32;
        let environment = encode_environment(
            environment_map
                .iter()
                .map(|(key, value)| (key.as_ref(), value.as_os_str())),
        )?;

        let mut inherited = Vec::new();
        let mut namespace = 0u32;
        for index in 0..MAX_CAPABILITIES {
            let Some((role, handle, rights)) = crate::sys::pal::rustos_startup_capability(index)
            else {
                continue;
            };
            if role == ROLE_EXECUTABLE_NAMESPACE {
                namespace = handle;
            }
            if matches!(role, ROLE_STDIN | ROLE_STDOUT | ROLE_STDERR)
                || rights & RIGHT_TRANSFER == 0
            {
                continue;
            }
            inherited.push((role, handle, rights));
        }
        inherited.extend(extra);
        if namespace == 0 || inherited.len() > MAX_CAPABILITIES {
            return Err(io::const_error!(
                io::ErrorKind::PermissionDenied,
                "missing executable namespace"
            ));
        }
        let mut transfers = [SpawnCapability {
            source: 0,
            target_slot: 0,
            role: 0,
            rights: 0,
        }; MAX_CAPABILITIES];
        for (index, (role, handle, rights)) in inherited.iter().copied().enumerate() {
            transfers[index] = SpawnCapability {
                source: handle,
                target_slot: (index + 1) as u16,
                role,
                rights,
            };
        }
        let request = ProcessSpawnRequest {
            version: PROCESS_ABI_VERSION,
            flags: 0,
            path_address: path.as_ptr().addr() as u64,
            path_length: path.len() as u32,
            priority: PRIORITY_INTERACTIVE,
            reserved0: [0; 3],
            arguments_address: arguments.as_ptr().addr() as u64,
            arguments_length: arguments.len() as u32,
            argument_count: execution_args.len() as u32,
            environment_address: environment.as_ptr().addr() as u64,
            environment_length: environment.len() as u32,
            environment_count,
            capabilities_address: transfers.as_ptr().addr() as u64,
            capability_count: inherited.len() as u32,
            namespace,
        };
        let mut result = ProcessSpawnResult {
            process: 0,
            reserved: 0,
            pid: 0,
        };
        let status = unsafe {
            crate::sys::pal::syscall3(
                5,
                ptr::from_ref(&request).addr() as u64,
                ptr::from_mut(&mut result).addr() as u64,
                0,
            )
        };
        if status != STATUS_OK {
            return Err(io::const_error!(
                io::ErrorKind::NotFound,
                "process_spawn failed"
            ));
        }
        drop(child_pipes);
        Ok((
            Process {
                handle: result.process,
                pid: result.pid,
                exit: None,
            },
            parent_pipes,
        ))
    }
}

fn prepare_stream(
    stdio: Stdio,
    role: u16,
    child_reads: bool,
    parent: &mut Option<Pipe>,
    child_pipes: &mut Vec<Pipe>,
    capabilities: &mut Vec<(u16, u32, u64)>,
) -> io::Result<()> {
    match stdio {
        Stdio::Null => Ok(()),
        Stdio::MakePipe => {
            let (reader, writer) = pipe::pipe()?;
            let (child, parent_pipe) = if child_reads {
                (reader, writer)
            } else {
                (writer, reader)
            };
            capabilities.push((role, child.handle(), child.rights()));
            child_pipes.push(child);
            *parent = Some(parent_pipe);
            Ok(())
        }
        Stdio::Pipe(pipe) => {
            capabilities.push((role, pipe.handle(), pipe.rights()));
            child_pipes.push(pipe);
            Ok(())
        }
        Stdio::Inherit | Stdio::ParentStdout | Stdio::ParentStderr => {
            let source_role = match stdio {
                Stdio::ParentStdout => ROLE_STDOUT,
                Stdio::ParentStderr => ROLE_STDERR,
                _ => role,
            };
            if let Some((_, handle, rights)) = startup_by_role(source_role) {
                capabilities.push((role, handle, rights));
            }
            Ok(())
        }
        Stdio::InheritFile(_) => Err(io::const_error!(
            io::ErrorKind::Unsupported,
            "passing VFS File as stdio requires a stream adapter",
        )),
    }
}

fn startup_by_role(role: u16) -> Option<(u16, u32, u64)> {
    (0..MAX_CAPABILITIES)
        .filter_map(crate::sys::pal::rustos_startup_capability)
        .find(|capability| capability.0 == role && capability.2 & RIGHT_TRANSFER != 0)
}

fn resolve_stdio(value: Option<Stdio>, default: DefaultStdio) -> Stdio {
    value.unwrap_or(match default {
        DefaultStdio::Inherit => Stdio::Inherit,
        DefaultStdio::Null => Stdio::Null,
        DefaultStdio::MakePipe => Stdio::MakePipe,
    })
}

fn default_kind(value: &Stdio) -> io::Result<DefaultStdio> {
    match value {
        Stdio::Inherit => Ok(DefaultStdio::Inherit),
        Stdio::Null => Ok(DefaultStdio::Null),
        Stdio::MakePipe => Ok(DefaultStdio::MakePipe),
        _ => Err(io::const_error!(
            io::ErrorKind::InvalidInput,
            "invalid default stdio"
        )),
    }
}

fn executable_plan(
    program: &OsStr,
    original_args: &[OsString],
    requested_cwd: Option<&OsStr>,
) -> io::Result<(Vec<u8>, Vec<OsString>)> {
    let program = program
        .to_str()
        .ok_or_else(|| io::const_error!(io::ErrorKind::InvalidInput, "program is not UTF-8"))?;
    if program.as_bytes().contains(&0) || program.is_empty() {
        return Err(io::const_error!(
            io::ErrorKind::InvalidInput,
            "invalid program path"
        ));
    }
    if !program.contains('/') {
        let mut path = b"/boot/system/bin/".to_vec();
        path.extend_from_slice(program.as_bytes());
        if !program.ends_with(".rune") {
            path.extend_from_slice(b".rune");
        }
        return Ok((path, original_args.to_vec()));
    }

    let unresolved = if Path::new(program).is_absolute() {
        PathBuf::from(program)
    } else if let Some(cwd) = requested_cwd {
        crate::sys::paths::rustos::absolute(Path::new(cwd))?.join(program)
    } else {
        crate::sys::paths::rustos::absolute(Path::new(program))?
    };
    let target = crate::sys::paths::rustos::absolute(&unresolved)?;
    let target = target.to_str().ok_or_else(|| {
        io::const_error!(io::ErrorKind::InvalidInput, "executable path is not UTF-8")
    })?;
    if target.starts_with("/boot/") {
        return Ok((target.as_bytes().to_vec(), original_args.to_vec()));
    }

    // Kernel остаётся свободен от parser'а постоянной ФС. Он запускает только
    // маленький проверяемый runner из initramfs; runner читает target/DLL из
    // vfsd, разрешает RUNE imports и передаёт target исходный argv.
    let mut arguments = Vec::with_capacity(original_args.len() + 1);
    arguments.push(OsString::from("rune-runner"));
    arguments.push(OsString::from(target));
    arguments.extend(original_args.iter().skip(1).cloned());
    Ok((b"/boot/system/bin/rune-runner.rune".to_vec(), arguments))
}

fn encode_strings<'a>(strings: impl Iterator<Item = &'a OsStr>) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    for string in strings {
        let string = string.to_str().ok_or_else(|| {
            io::const_error!(io::ErrorKind::InvalidInput, "argument is not UTF-8")
        })?;
        if string.as_bytes().contains(&0) {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "argument contains NUL"
            ));
        }
        result.extend_from_slice(string.as_bytes());
        result.push(0);
    }
    if result.len() > MAX_TABLE_BYTES {
        return Err(io::const_error!(
            io::ErrorKind::ArgumentListTooLong,
            "argument table is too large"
        ));
    }
    Ok(result)
}

fn encode_environment<'a>(
    variables: impl Iterator<Item = (&'a OsStr, &'a OsStr)>,
) -> io::Result<Vec<u8>> {
    let mut result = Vec::new();
    for (name, value) in variables {
        let name = name.to_str().ok_or_else(|| {
            io::const_error!(io::ErrorKind::InvalidInput, "environment key is not UTF-8")
        })?;
        let value = value.to_str().ok_or_else(|| {
            io::const_error!(
                io::ErrorKind::InvalidInput,
                "environment value is not UTF-8"
            )
        })?;
        if name.is_empty()
            || name.as_bytes().contains(&b'=')
            || name.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
        {
            return Err(io::const_error!(
                io::ErrorKind::InvalidInput,
                "invalid environment"
            ));
        }
        result.extend_from_slice(name.as_bytes());
        result.push(b'=');
        result.extend_from_slice(value.as_bytes());
        result.push(0);
    }
    if result.len() > MAX_TABLE_BYTES {
        return Err(io::const_error!(
            io::ErrorKind::ArgumentListTooLong,
            "environment is too large"
        ));
    }
    Ok(result)
}

pub fn output(command: &mut Command) -> io::Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let (mut process, mut pipes) = command.spawn(Stdio::MakePipe, false)?;
    drop(pipes.stdin.take());

    // Нельзя читать stdout и stderr последовательно: ребёнок может сначала
    // заполнить второй 4-КиБ pipe и ждать reader, пока родитель ждёт EOF в
    // первом. Два независимых reader-потока дают ту же семантику, что и
    // Unix/Windows backend std, и важны для многословных rustc/build scripts.
    let stdout_reader = pipes.stdout.take().map(|pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    // Текущий поток сам обслуживает stderr. Одного worker достаточно для
    // одновременного дренирования двух pipes и это экономит стек/Thread cap.
    let mut stderr = Vec::new();
    if let Some(pipe) = pipes.stderr.take() {
        pipe.read_to_end(&mut stderr)?;
    }
    // Сначала дожидаемся EOF обоих streams, затем reap процесса. Так kernel
    // сохраняет process capability и endpoint ownership до завершения I/O;
    // pipe close на exit всё равно гарантированно будит оба reader'а.
    let stdout = join_reader(stdout_reader)?;
    let status = process.wait()?;
    Ok((status, stdout, stderr))
}

fn join_reader(reader: Option<thread::JoinHandle<io::Result<Vec<u8>>>>) -> io::Result<Vec<u8>> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| io::const_error!(io::ErrorKind::Other, "stdio reader panicked"))?,
        None => Ok(Vec::new()),
    }
}

impl From<ChildPipe> for Stdio {
    fn from(pipe: ChildPipe) -> Stdio {
        Stdio::Pipe(pipe)
    }
}
impl From<io::Stdout> for Stdio {
    fn from(_: io::Stdout) -> Stdio {
        Stdio::ParentStdout
    }
}
impl From<io::Stderr> for Stdio {
    fn from(_: io::Stderr) -> Stdio {
        Stdio::ParentStderr
    }
}
impl From<File> for Stdio {
    fn from(file: File) -> Stdio {
        Stdio::InheritFile(file)
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env)
            .field("cwd", &self.cwd)
            .finish()
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub struct ExitStatus(ExitReason);

impl ExitStatus {
    pub fn exit_ok(&self) -> Result<(), ExitStatusError> {
        match NonZero::new(self.0.status) {
            None if self.0.exception == 0 => Ok(()),
            code => Err(ExitStatusError {
                status: *self,
                code,
            }),
        }
    }
    pub fn code(&self) -> Option<i32> {
        (self.0.exception == 0).then_some(self.0.status)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.exception == 0 {
            write!(formatter, "exit code: {}", self.0.status)
        } else {
            write!(
                formatter,
                "exception {} at {:#x}",
                self.0.exception, self.0.fault_address
            )
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatusError {
    status: ExitStatus,
    code: Option<NonZero<i32>>,
}
impl From<ExitStatusError> for ExitStatus {
    fn from(error: ExitStatusError) -> ExitStatus {
        error.status
    }
}
impl ExitStatusError {
    pub fn code(self) -> Option<NonZero<i32>> {
        self.code
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct ExitCode(u8);
impl ExitCode {
    pub const SUCCESS: ExitCode = ExitCode(0);
    pub const FAILURE: ExitCode = ExitCode(1);
    pub fn as_i32(&self) -> i32 {
        self.0 as i32
    }
}
impl From<u8> for ExitCode {
    fn from(code: u8) -> Self {
        Self(code)
    }
}

pub struct Process {
    handle: u32,
    pid: u64,
    exit: Option<ExitStatus>,
}
impl Process {
    pub fn id(&self) -> u32 {
        self.pid as u32
    }
    pub fn kill(&mut self) -> io::Result<()> {
        let result = unsafe { crate::sys::pal::syscall3(7, self.handle as u64, 1u64, 0) };
        if result == STATUS_OK {
            Ok(())
        } else {
            Err(io::const_error!(
                io::ErrorKind::Other,
                "process_kill failed"
            ))
        }
    }
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.exit {
            return Ok(status);
        }
        let mut reason = ExitReason::default();
        let result = unsafe {
            crate::sys::pal::syscall3(
                6,
                self.handle as u64,
                ptr::from_mut(&mut reason).addr() as u64,
                0,
            )
        };
        if result != STATUS_OK {
            return Err(io::const_error!(
                io::ErrorKind::Other,
                "process_wait failed"
            ));
        }
        let status = ExitStatus(reason);
        self.exit = Some(status);
        Ok(status)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exit {
            return Ok(Some(status));
        }
        let mut reason = ExitReason::default();
        let result = unsafe {
            crate::sys::pal::syscall3(
                31,
                self.handle as u64,
                ptr::from_mut(&mut reason).addr() as u64,
                0,
            )
        };
        if result == STATUS_BUSY {
            return Ok(None);
        }
        if result != STATUS_OK {
            return Err(io::const_error!(
                io::ErrorKind::Other,
                "process_try_wait failed"
            ));
        }
        let status = ExitStatus(reason);
        self.exit = Some(status);
        Ok(Some(status))
    }
}

pub struct CommandArgs<'a> {
    iter: crate::slice::Iter<'a, OsString>,
}
impl<'a> Iterator for CommandArgs<'a> {
    type Item = &'a OsStr;
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|value| &**value)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}
impl ExactSizeIterator for CommandArgs<'_> {
    fn len(&self) -> usize {
        self.iter.len()
    }
    fn is_empty(&self) -> bool {
        self.iter.is_empty()
    }
}
impl fmt::Debug for CommandArgs<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter.clone()).finish()
    }
}

pub type ChildPipe = Pipe;
pub fn read_output(
    out: ChildPipe,
    stdout: &mut Vec<u8>,
    err: ChildPipe,
    stderr: &mut Vec<u8>,
) -> io::Result<()> {
    thread::scope(|scope| {
        // Один worker читает stdout, а вызывающий поток одновременно читает
        // stderr. Два дополнительных потока здесь не дают выигрыша, зато
        // расходуют второй 256-КиБ stack и усложняют lifecycle при ошибке.
        let out_reader = scope.spawn(move || out.read_to_end(stdout));
        let err_result = err.read_to_end(stderr);
        out_reader
            .join()
            .map_err(|_| io::const_error!(io::ErrorKind::Other, "stdout reader panicked"))??;
        err_result?;
        Ok(())
    })
}
pub fn getpid() -> u32 {
    crate::sys::pal::rustos_process_id() as u32
}
