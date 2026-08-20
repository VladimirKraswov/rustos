//! Capability pipes RustOS для stdio и `Command::output`.

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::ptr;

const RIGHT_READ: u64 = 1;
const RIGHT_WRITE: u64 = 2;
const RIGHT_TRANSFER: u64 = 1 << 9;
const STATUS_BUSY: i64 = -11;

#[repr(C)]
struct PipeCreateResult {
    reader: u32,
    writer: u32,
    version: u32,
    reserved: u32,
}

pub struct Pipe {
    handle: u32,
    rights: u64,
}

pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let mut result = PipeCreateResult {
        reader: 0,
        writer: 0,
        version: 0,
        reserved: 0,
    };
    let status =
        unsafe { crate::sys::pal::syscall3(27, ptr::from_mut(&mut result).addr() as u64, 0, 0) };
    if status != 0 || result.version != 1 {
        return Err(io::const_error!(io::ErrorKind::Other, "pipe_create failed"));
    }
    Ok((
        Pipe::from_handle(result.reader, RIGHT_READ | RIGHT_TRANSFER),
        Pipe::from_handle(result.writer, RIGHT_WRITE | RIGHT_TRANSFER),
    ))
}

impl Pipe {
    pub(crate) const fn from_handle(handle: u32, rights: u64) -> Self {
        Self { handle, rights }
    }

    pub(crate) const fn handle(&self) -> u32 {
        self.handle
    }

    pub(crate) const fn rights(&self) -> u64 {
        self.rights
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        let handle = unsafe { crate::sys::pal::syscall3(30, self.handle as u64, self.rights, 0) };
        if handle > 0 {
            Ok(Self::from_handle(handle as u32, self.rights))
        } else {
            Err(io::const_error!(
                io::ErrorKind::Other,
                "pipe duplicate failed"
            ))
        }
    }

    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let result = unsafe {
                crate::sys::pal::syscall3(
                    28,
                    self.handle as u64,
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
                    "pipe read failed"
                ));
            }
        }
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        let capacity = cursor.capacity();
        // SAFETY: BorrowedCursor гарантирует capacity writable байт.
        let destination = unsafe {
            core::slice::from_raw_parts_mut(cursor.as_mut().as_mut_ptr().cast::<u8>(), capacity)
        };
        let read = self.read(destination)?;
        // SAFETY: pipe syscall инициализировал ровно `read` байт.
        unsafe { cursor.advance(read) };
        Ok(())
    }

    pub fn read_vectored(&self, buffers: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let Some(buffer) = buffers.iter_mut().find(|buffer| !buffer.is_empty()) else {
            return Ok(0);
        };
        self.read(buffer)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, output: &mut Vec<u8>) -> io::Result<usize> {
        let before = output.len();
        let mut buffer = [0u8; 1024];
        loop {
            let read = self.read(&mut buffer)?;
            if read == 0 {
                return Ok(output.len() - before);
            }
            output.extend_from_slice(&buffer[..read]);
        }
    }

    pub fn write(&self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            let result = unsafe {
                crate::sys::pal::syscall3(
                    29,
                    self.handle as u64,
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
                    "pipe write failed"
                ));
            }
        }
    }

    pub fn write_vectored(&self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
        let Some(buffer) = buffers.iter().find(|buffer| !buffer.is_empty()) else {
            return Ok(0);
        };
        self.write(buffer)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn diverge(&self) -> ! {
        crate::sys::pal::abort_internal()
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { crate::sys::pal::syscall3(17, self.handle as u64, 0, 0) };
            self.handle = 0;
        }
    }
}

impl fmt::Debug for Pipe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Pipe")
            .field("handle", &self.handle)
            .field("rights", &self.rights)
            .finish()
    }
}
