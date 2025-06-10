#[cfg(not(feature = "loom"))]
pub(crate) use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "loom")]
pub(crate) use loom::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(feature = "loom"))]
pub(crate) struct Spinlock<T> {
    lock: AtomicU64,
    value: std::cell::UnsafeCell<T>,
}

#[cfg(not(feature = "loom"))]
unsafe impl<T: Send> Sync for Spinlock<T> {}

#[cfg(not(feature = "loom"))]
unsafe impl<T: Send> Send for Spinlock<T> {}

#[cfg(not(feature = "loom"))]
pub(crate) struct SpinlockGuard<'a, T> {
    spinlock: &'a Spinlock<T>,
}

#[cfg(not(feature = "loom"))]
impl<T> Spinlock<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            lock: AtomicU64::new(0),
            value: std::cell::UnsafeCell::new(value),
        }
    }

    #[inline(always)]
    pub(crate) fn lock(&self) -> SpinlockGuard<T> {
        loop {
            if self.lock.load(Ordering::Relaxed) == 1 {
                continue;
            }
            if self
                .lock
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
            std::hint::spin_loop();
        }
        SpinlockGuard { spinlock: self }
    }
}

#[cfg(not(feature = "loom"))]
impl<'a, T> std::ops::Deref for SpinlockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.spinlock.value.get() }
    }
}

#[cfg(not(feature = "loom"))]
impl<'a, T> std::ops::DerefMut for SpinlockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.spinlock.value.get() }
    }
}

#[cfg(not(feature = "loom"))]
impl<'a, T> Drop for SpinlockGuard<'a, T> {
    fn drop(&mut self) {
        self.spinlock.lock.store(0, Ordering::Release);
    }
}

#[cfg(feature = "loom")]
pub(crate) struct Spinlock<T> {
    inner: loom::sync::Mutex<T>,
}

#[cfg(feature = "loom")]
impl<T> Spinlock<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: loom::sync::Mutex::new(value),
        }
    }

    pub(crate) fn lock(&self) -> impl std::ops::Deref<Target = T> + '_ {
        self.inner.lock().unwrap()
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

    pub(crate) struct Notification {
        eventfd: EventFd,
    }

    impl Notification {
        pub(crate) fn new() -> Result<Self, MpscBufError> {
            let eventfd = EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC)
                .map_err(|e| MpscBufError::EventfdCreation(e.to_string()))?;

            Ok(Notification { eventfd })
        }

        /// # Safety
        ///
        /// The caller must ensure that `fd` is a valid eventfd file descriptor.
        /// The file descriptor will be owned by this Notification instance.
        pub(crate) unsafe fn from_owned_fd(fd: OwnedFd) -> Self {
            let eventfd = EventFd::from_owned_fd(fd);
            Notification { eventfd }
        }

        pub(crate) fn notify(&self) -> Result<(), MpscBufError> {
            self.eventfd
                .write(1)
                .map_err(|e| MpscBufError::EventfdWrite(e.to_string()))?;
            Ok(())
        }

        pub(crate) fn wait(&self) -> Result<(), MpscBufError> {
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

        pub(crate) fn notify(&self) -> Result<(), MpscBufError> {
            let mut notified = self.inner.mutex.lock().unwrap();
            *notified = true;
            self.inner.condvar.notify_one();
            Ok(())
        }

        pub(crate) fn wait(&self) -> Result<(), MpscBufError> {
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
