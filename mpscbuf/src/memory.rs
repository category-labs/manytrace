use crate::error::MpscBufError;
use core::ptr::NonNull;
use eyre::{ensure, Result, WrapErr};
use nix::sys::memfd::{memfd_create, MFdFlags};
use nix::sys::mman::{mmap, mmap_anonymous, munmap, MapFlags, ProtFlags};
use nix::unistd::ftruncate;
use std::num::NonZero;

pub struct Memory {
    ptr: NonNull<u8>,
    size: usize,
    page_size: usize,
    fd: std::os::fd::OwnedFd,
}

impl Memory {
    pub fn new(size: usize) -> Result<Self> {
        let page_size = get_page_size();
        ensure!(
            size % page_size == 0,
            MpscBufError::SizeNotAligned(page_size)
        );
        ensure!(size >= 2 * page_size, MpscBufError::SizeTooSmall(page_size));

        let fd = memfd_create(c"mpscbuf", MFdFlags::MFD_CLOEXEC)
            .wrap_err("failed to create memory file descriptor")?;

        ftruncate(&fd, size as i64).wrap_err("failed to set memory file size")?;

        let total_size = size * 2;
        let ptr = unsafe {
            mmap_anonymous(
                None,
                NonZero::new(total_size).unwrap(),
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            )
            .wrap_err("failed to allocate virtual memory space")?
        };

        unsafe {
            mmap(
                Some(NonZero::new(ptr.as_ptr() as usize).unwrap()),
                NonZero::new(size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                &fd,
                0,
            )
            .wrap_err("failed to map first half of ring buffer")?;
        }

        unsafe {
            mmap(
                Some(NonZero::new(ptr.as_ptr().add(size) as usize).unwrap()),
                NonZero::new(size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                &fd,
                0,
            )
            .wrap_err("failed to map second half of ring buffer")?;
        }

        let ptr = NonNull::new(ptr.as_ptr() as *mut u8).expect("mmap returned null pointer");

        Ok(Memory {
            ptr,
            size,
            page_size,
            fd,
        })
    }

    pub fn as_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn page_size(&self) -> usize {
        self.page_size
    }

    pub fn metadata_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    pub fn data_ptr(&self) -> NonNull<u8> {
        unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(self.page_size)) }
    }

    pub fn data_size(&self) -> usize {
        self.size - self.page_size
    }

    pub fn size_mask(&self) -> usize {
        self.size - 1
    }

    pub fn fd(&self) -> &std::os::fd::OwnedFd {
        &self.fd
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        unsafe {
            let _ = munmap(
                NonNull::new(self.ptr.as_ptr() as *mut _).unwrap(),
                self.size * 2,
            );
        }
    }
}

unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

fn get_page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_continuous_mapping() -> Result<()> {
        let page_size = get_page_size();
        let size = page_size * 2;
        let memory = Memory::new(size)?;

        let ptr = memory.as_ptr().as_ptr();
        
        unsafe {
            for i in 0..size {
                let byte_value = (i % 256) as u8;
                ptr.add(i).write(byte_value);
            }

            for i in 0..size {
                let expected = (i % 256) as u8;
                let actual = ptr.add(i).read();
                assert_eq!(actual, expected, "mismatch at position {}", i);
                
                let wrapped_actual = ptr.add(i + size).read();
                assert_eq!(wrapped_actual, expected, "mismatch at wrapped position {}", i + size);
            }
        }

        Ok(())
    }

    #[test]
    fn test_wrap_around_write() -> Result<()> {
        let page_size = get_page_size();
        let size = page_size * 2;
        let memory = Memory::new(size)?;

        let ptr = memory.as_ptr().as_ptr();
        let pattern = b"ABCDEFGH";
        
        unsafe {
            let start_pos = size - pattern.len() / 2;
            for (i, &byte) in pattern.iter().enumerate() {
                ptr.add(start_pos + i).write(byte);
            }

            for (i, &expected) in pattern.iter().enumerate() {
                let actual = ptr.add(start_pos + i).read();
                assert_eq!(actual, expected, "mismatch at position {}", start_pos + i);
            }

            for (i, &expected) in pattern[pattern.len() / 2..].iter().enumerate() {
                let actual = ptr.add(i).read();
                assert_eq!(actual, expected, "mismatch at wrapped position {}", i);
            }
        }

        Ok(())
    }

    #[test]
    fn test_metadata_and_data_regions() -> Result<()> {
        let page_size = get_page_size();
        let size = page_size * 4;
        let memory = Memory::new(size)?;

        assert_eq!(memory.metadata_ptr(), memory.as_ptr());
        assert_eq!(memory.data_size(), size - page_size);
        
        unsafe {
            let metadata_ptr = memory.metadata_ptr().as_ptr();
            let data_ptr = memory.data_ptr().as_ptr();
            
            metadata_ptr.write(0xAA);
            data_ptr.write(0xBB);
            
            assert_eq!(metadata_ptr.read(), 0xAA);
            assert_eq!(data_ptr.read(), 0xBB);
            assert_eq!(metadata_ptr.offset_from(data_ptr).abs() as usize, page_size);
        }

        Ok(())
    }
}
