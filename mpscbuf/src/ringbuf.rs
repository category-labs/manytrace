use crate::{common::Metadata, memory::Memory, sync::Ordering, MpscBufError};
use eyre::Result;
use std::os::fd::AsFd;

pub(crate) struct RingBuf {
    memory: Memory,
}

impl RingBuf {
    pub(crate) fn new(data_size: usize) -> Result<Self, MpscBufError> {
        let memory = Memory::new(data_size)
            .map_err(|_| MpscBufError::MmapFailed(nix::errno::Errno::EINVAL))?;

        let metadata_ptr = memory.metadata_ptr().as_ptr() as *mut Metadata;
        unsafe {
            metadata_ptr.write(Metadata::new());
        }
        Ok(RingBuf { memory })
    }

    pub fn from_fd(fd: std::os::fd::OwnedFd, data_size: usize) -> Result<Self, MpscBufError> {
        let memory = Memory::from_fd(fd, data_size)
            .map_err(|_| MpscBufError::MmapFailed(nix::errno::Errno::EINVAL))?;
        Ok(RingBuf { memory })
    }

    pub(crate) fn metadata(&self) -> &Metadata {
        unsafe { &*(self.memory.metadata_ptr().as_ptr() as *const Metadata) }
    }

    pub(crate) fn data_size(&self) -> usize {
        self.memory.data_size()
    }

    pub(crate) fn size_mask(&self) -> u64 {
        (self.memory.data_size() - 1) as u64
    }

    pub(crate) fn data_ptr(&self) -> *mut u8 {
        self.memory.data_ptr().as_ptr()
    }

    pub(crate) fn consumer_pos(&self) -> u64 {
        self.metadata().consumer.load(Ordering::Acquire)
    }

    pub(crate) fn producer_pos(&self) -> u64 {
        self.metadata().producer.load(Ordering::Acquire)
    }

    pub(crate) fn advance_producer(&self, amount: u64) {
        self.metadata().producer.store(amount, Ordering::Release);
    }

    pub(crate) fn advance_consumer(&self, amount: u64) {
        self.metadata().consumer.store(amount, Ordering::Release);
    }

    pub fn increment_dropped(&self) {
        self.metadata().dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dropped(&self) -> u64 {
        self.metadata().dropped.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn clone_fd(&self) -> Result<std::os::fd::OwnedFd, MpscBufError> {
        self.memory
            .clone_fd()
            .map_err(|_| MpscBufError::MmapFailed(nix::errno::Errno::EINVAL))
    }

    pub fn memory_fd(&self) -> std::os::fd::BorrowedFd {
        self.memory.fd().as_fd()
    }
}

unsafe impl Send for RingBuf {}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::*;

    #[fixture]
    fn ringbuf() -> RingBuf {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 2;
        RingBuf::new(size).unwrap()
    }

    #[rstest]
    fn test_ringbuf_creation(ringbuf: RingBuf) -> Result<()> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 2;

        assert_eq!(ringbuf.data_size(), size);
        assert_eq!(ringbuf.consumer_pos(), 0);
        assert_eq!(ringbuf.producer_pos(), 0);
        assert_eq!(ringbuf.dropped(), 0);
        assert_eq!(ringbuf.size_mask(), (size - 1) as u64);

        Ok(())
    }

    #[rstest]
    fn test_position_updates(ringbuf: RingBuf) {
        assert_eq!(ringbuf.producer_pos(), 0);
        assert_eq!(ringbuf.consumer_pos(), 0);

        ringbuf.advance_producer(100);
        assert_eq!(ringbuf.producer_pos(), 100);
        assert_eq!(ringbuf.consumer_pos(), 0);

        ringbuf.advance_consumer(50);
        assert_eq!(ringbuf.producer_pos(), 100);
        assert_eq!(ringbuf.consumer_pos(), 50);
    }

    #[rstest]
    fn test_dropped_counter(ringbuf: RingBuf) {
        assert_eq!(ringbuf.dropped(), 0);

        ringbuf.increment_dropped();
        assert_eq!(ringbuf.dropped(), 1);

        ringbuf.increment_dropped();
        assert_eq!(ringbuf.dropped(), 2);
    }
}
