#[cfg(not(feature = "loom"))]
pub use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "loom")]
pub use loom::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(feature = "loom"))]
pub use spinning_top::Spinlock;

#[cfg(feature = "loom")]
pub struct Spinlock<T> {
    inner: loom::sync::Mutex<T>,
}

#[cfg(feature = "loom")]
impl<T> Spinlock<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: loom::sync::Mutex::new(value),
        }
    }

    pub fn lock(&self) -> impl std::ops::Deref<Target = T> + '_ {
        self.inner.lock().unwrap()
    }

    pub fn try_lock(&self) -> Option<impl std::ops::Deref<Target = T> + '_> {
        self.inner.try_lock().ok()
    }
}

#[cfg(not(feature = "loom"))]
pub mod notification {
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
}

#[cfg(feature = "loom")]
pub mod notification {
    use crate::error::MpscBufError;
    use loom::sync::{Condvar, Mutex};
    use std::os::{fd::BorrowedFd, unix::io::OwnedFd};
    use std::sync::Arc;

    #[derive(Clone)]
    pub struct Notification {
        inner: Arc<NotificationInner>,
    }

    struct NotificationInner {
        condvar: Condvar,
        mutex: Mutex<bool>,
    }

    impl Notification {
        pub fn new() -> Result<Self, MpscBufError> {
            Ok(Notification {
                inner: Arc::new(NotificationInner {
                    condvar: Condvar::new(),
                    mutex: Mutex::new(false),
                }),
            })
        }

        /// # Safety
        ///
        /// This function is not supported in loom mode and will panic.
        pub unsafe fn from_owned_fd(_fd: OwnedFd) -> Self {
            panic!("from_owned_fd() not supported in loom mode")
        }

        pub fn notify(&self) -> Result<(), MpscBufError> {
            let mut notified = self.inner.mutex.lock().unwrap();
            *notified = true;
            self.inner.condvar.notify_one();
            Ok(())
        }

        pub fn wait(&self) -> Result<(), MpscBufError> {
            let mut notified = self.inner.mutex.lock().unwrap();
            while !*notified {
                notified = self.inner.condvar.wait(notified).unwrap();
            }
            *notified = false;
            Ok(())
        }

        pub fn fd(&self) -> BorrowedFd {
            panic!("fd() not supported in loom mode")
        }
    }
}
