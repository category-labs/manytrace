use crate::error::MpscBufError;
use nix::sys::eventfd::{EfdFlags, EventFd};
use std::os::{
    fd::{AsFd, BorrowedFd},
    unix::io::OwnedFd,
};

pub struct Notification {
    eventfd: EventFd,
}

impl Notification {
    pub fn new() -> Result<Self, MpscBufError> {
        let eventfd = EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC)
            .map_err(|e| MpscBufError::EventfdCreation(e.to_string()))?;

        Ok(Notification { eventfd })
    }

    /// Creates a Notification from an existing file descriptor.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `fd` is a valid eventfd file descriptor.
    /// The file descriptor will be owned by this Notification instance.
    pub unsafe fn from_owned_fd(fd: OwnedFd) -> Self {
        let eventfd = EventFd::from_owned_fd(fd);
        Notification { eventfd }
    }

    pub fn notify(&self) -> Result<(), MpscBufError> {
        self.eventfd
            .write(1)
            .map_err(|e| MpscBufError::EventfdWrite(e.to_string()))?;
        Ok(())
    }

    pub fn wait(&self) -> Result<(), MpscBufError> {
        self.eventfd
            .read()
            .map_err(|e| MpscBufError::EventfdRead(e.to_string()))?;
        Ok(())
    }

    pub fn fd(&self) -> BorrowedFd {
        self.eventfd.as_fd()
    }
}
