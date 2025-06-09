use crate::error::MpscBufError;
use core::ptr::NonNull;
use eyre::{ensure, Result, WrapErr};
use nix::sys::memfd::{memfd_create, MFdFlags};
use nix::sys::mman::{mmap, mmap_anonymous, munmap, MapFlags, ProtFlags};
use nix::unistd::ftruncate;
use std::num::NonZero;
use std::os::unix::io::AsFd;

pub(crate) struct Memory {
    ptr: NonNull<u8>,
    data_size: usize,
    page_size: usize,
    fd: std::os::fd::OwnedFd,
}

impl Memory {
    pub(crate) fn new(data_size: usize) -> Result<Self> {
        let page_size = get_page_size();

        ensure!(
            data_size.is_power_of_two(),
            MpscBufError::SizeNotAligned(data_size)
        );
        ensure!(
            data_size >= page_size,
            MpscBufError::SizeTooSmall(page_size)
        );

        let total_size = data_size + page_size;

        let fd = memfd_create(c"mpscbuf", MFdFlags::MFD_CLOEXEC)
            .wrap_err("failed to create memory file descriptor")?;

        ftruncate(&fd, total_size as i64).wrap_err("failed to set memory file size")?;

        Self::from_fd(fd, data_size)
    }

    pub(crate) fn from_fd(fd: std::os::fd::OwnedFd, data_size: usize) -> Result<Self> {
        let page_size = get_page_size();

        ensure!(
            data_size.is_power_of_two(),
            MpscBufError::SizeNotAligned(data_size)
        );
        ensure!(
            data_size >= page_size,
            MpscBufError::SizeTooSmall(page_size)
        );

        let total_size = data_size + page_size;
        let total_virtual_size = page_size + 2 * data_size;

        let ptr = unsafe {
            mmap_anonymous(
                None,
                NonZero::new(total_virtual_size).unwrap(),
                ProtFlags::PROT_NONE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS,
            )
            .wrap_err("failed to allocate virtual memory space")?
        };

        unsafe {
            mmap(
                Some(NonZero::new(ptr.as_ptr() as usize).unwrap()),
                NonZero::new(total_size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                &fd,
                0,
            )
            .wrap_err("failed to map metadata and first data region")?;
        }

        unsafe {
            mmap(
                Some(NonZero::new(ptr.as_ptr().add(total_size) as usize).unwrap()),
                NonZero::new(data_size).unwrap(),
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED | MapFlags::MAP_FIXED,
                &fd,
                page_size as i64,
            )
            .wrap_err("failed to map second data region")?;
        }

        let ptr = NonNull::new(ptr.as_ptr() as *mut u8).expect("mmap returned null pointer");

        let memory = Memory {
            ptr,
            data_size,
            page_size,
            fd,
        };
        Ok(memory)
    }

    #[cfg(test)]
    fn as_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    pub(crate) fn metadata_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    pub(crate) fn data_ptr(&self) -> NonNull<u8> {
        unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(self.page_size)) }
    }

    pub(crate) fn data_size(&self) -> usize {
        self.data_size
    }

    pub(crate) fn fd(&self) -> &std::os::fd::OwnedFd {
        &self.fd
    }

    pub(crate) fn clone_fd(&self) -> Result<std::os::fd::OwnedFd> {
        self.fd
            .as_fd()
            .try_clone_to_owned()
            .wrap_err("failed to clone memory file descriptor")
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        unsafe {
            let total_virtual_size = self.page_size + 2 * self.data_size;
            let _ = munmap(
                NonNull::new(self.ptr.as_ptr() as *mut _).unwrap(),
                total_virtual_size,
            );
        }
    }
}

unsafe impl Send for Memory {}

fn get_page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_continuous_mapping() -> Result<()> {
        let page_size = get_page_size();
        let data_size = page_size * 2;
        let memory = Memory::new(data_size)?;

        let metadata_ptr = memory.metadata_ptr().as_ptr();
        let data_ptr = memory.data_ptr().as_ptr();
        let data_size = memory.data_size();

        unsafe {
            for i in 0..page_size {
                let byte_value = (i % 256) as u8;
                metadata_ptr.add(i).write(byte_value);
            }

            for i in 0..data_size {
                let byte_value = ((i + 100) % 256) as u8;
                data_ptr.add(i).write(byte_value);
            }

            for i in 0..page_size {
                let expected = (i % 256) as u8;
                let actual = metadata_ptr.add(i).read();
                assert_eq!(actual, expected, "mismatch at metadata position {}", i);
            }

            for i in 0..data_size {
                let expected = ((i + 100) % 256) as u8;
                let actual = data_ptr.add(i).read();
                assert_eq!(actual, expected, "mismatch at data position {}", i);

                let wrapped_actual = data_ptr.add(i + data_size).read();
                assert_eq!(
                    wrapped_actual,
                    expected,
                    "mismatch at wrapped data position {}",
                    i + data_size
                );
            }
        }

        Ok(())
    }

    #[test]
    fn test_wrap_around_write() -> Result<()> {
        let page_size = get_page_size();
        let data_size = page_size * 2;
        let memory = Memory::new(data_size)?;

        let data_ptr = memory.data_ptr().as_ptr();
        let data_size = memory.data_size();
        let pattern = b"ABCDEFGH";

        unsafe {
            let start_pos = data_size - pattern.len() / 2;
            for (i, &byte) in pattern.iter().enumerate() {
                data_ptr.add(start_pos + i).write(byte);
            }

            for (i, &expected) in pattern.iter().enumerate() {
                let actual = data_ptr.add(start_pos + i).read();
                assert_eq!(actual, expected, "mismatch at position {}", start_pos + i);
            }

            for (i, &expected) in pattern.iter().enumerate() {
                let wrapped_pos = (start_pos + i) % data_size;
                let actual = data_ptr.add(data_size + wrapped_pos).read();
                assert_eq!(
                    actual, expected,
                    "mismatch at wrapped position {}",
                    wrapped_pos
                );
            }
        }

        Ok(())
    }

    #[test]
    fn test_metadata_and_data_regions() -> Result<()> {
        let page_size = get_page_size();
        let data_size = page_size * 4;
        let memory = Memory::new(data_size)?;

        assert_eq!(memory.metadata_ptr(), memory.as_ptr());
        assert_eq!(memory.data_size(), data_size);

        unsafe {
            let metadata_ptr = memory.metadata_ptr().as_ptr();
            let data_ptr = memory.data_ptr().as_ptr();

            metadata_ptr.write(0xAA);
            data_ptr.write(0xBB);

            assert_eq!(metadata_ptr.read(), 0xAA);
            assert_eq!(data_ptr.read(), 0xBB);
            assert_eq!(metadata_ptr.offset_from(data_ptr).unsigned_abs(), page_size);
        }

        Ok(())
    }
}
