//! Standard streams поверх startup pipe capabilities.

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};

const ROLE_STDIN: u16 = 4;
const ROLE_STDOUT: u16 = 5;
const ROLE_STDERR: u16 = 6;
const STATUS_BUSY: i64 = -11;

pub struct Stdin;

pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(handle) = crate::sys::pal::rustos_startup_handle(ROLE_STDIN) else {
            return Ok(0);
        };
        pipe_read(handle, buffer)
    }

    fn read_buf(&mut self, mut cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        let capacity = cursor.capacity();
        let destination = unsafe {
            core::slice::from_raw_parts_mut(cursor.as_mut().as_mut_ptr().cast::<u8>(), capacity)
        };
        let read = self.read(destination)?;
        unsafe { cursor.advance(read) };
        Ok(())
    }

    fn read_vectored(&mut self, buffers: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let Some(buffer) = buffers.iter_mut().find(|buffer| !buffer.is_empty()) else {
            return Ok(0);
        };
        self.read(buffer)
    }

    fn is_read_vectored(&self) -> bool {
        false
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(handle) = crate::sys::pal::rustos_startup_handle(ROLE_STDOUT) else {
            // Ранние bootstrap-программы без stdio namespace сохраняют
            // прежнюю безопасную семантику sink, а не падают.
            return Ok(buffer.len());
        };
        pipe_write(handle, buffer)
    }

    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        let Some(buffer) = buffers.iter().find(|buffer| !buffer.is_empty()) else {
            return Ok(0);
        };
        self.write(buffer)
    }

    fn is_write_vectored(&self) -> bool {
        false
    }

    fn write_all(&mut self, mut buffer: &[u8]) -> io::Result<()> {
        while !buffer.is_empty() {
            let written = self.write(buffer)?;
            if written == 0 {
                return Err(io::Error::WRITE_ALL_EOF);
            }
            buffer = &buffer[written..];
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stderr {
    pub const fn new() -> Stderr {
        Stderr
    }
}

impl io::Write for Stderr {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(handle) = crate::sys::pal::rustos_startup_handle(ROLE_STDERR) else {
            return Ok(buffer.len());
        };
        pipe_write(handle, buffer)
    }

    fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        let Some(buffer) = buffers.iter().find(|buffer| !buffer.is_empty()) else {
            return Ok(0);
        };
        self.write(buffer)
    }

    fn is_write_vectored(&self) -> bool {
        false
    }

    fn write_all(&mut self, mut buffer: &[u8]) -> io::Result<()> {
        while !buffer.is_empty() {
            let written = self.write(buffer)?;
            if written == 0 {
                return Err(io::Error::WRITE_ALL_EOF);
            }
            buffer = &buffer[written..];
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub const STDIN_BUF_SIZE: usize = 8 * 1024;

pub fn is_ebadf(_error: &io::Error) -> bool {
    false
}

pub fn panic_output() -> Option<Vec<u8>> {
    None
}

fn pipe_read(handle: u32, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        let result = unsafe {
            crate::sys::pal::syscall3(
                28,
                handle as u64,
                buffer.as_mut_ptr().addr() as u64,
                buffer.len() as u64,
            )
        };
        if result >= 0 {
            return Ok(result as usize);
        }
        if result != STATUS_BUSY {
            return Err(io::const_error!(
                io::ErrorKind::BrokenPipe,
                "stdin pipe failed"
            ));
        }
    }
}

fn pipe_write(handle: u32, buffer: &[u8]) -> io::Result<usize> {
    loop {
        let result = unsafe {
            crate::sys::pal::syscall3(
                29,
                handle as u64,
                buffer.as_ptr().addr() as u64,
                buffer.len() as u64,
            )
        };
        if result >= 0 {
            return Ok(result as usize);
        }
        if result != STATUS_BUSY {
            return Err(io::const_error!(
                io::ErrorKind::BrokenPipe,
                "stdout pipe failed"
            ));
        }
    }
}
