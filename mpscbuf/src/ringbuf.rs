use crate::{Memory, Metadata};
use eyre::Result;
use std::sync::atomic::Ordering;

pub struct RingBuf {
    memory: Memory,
}

impl RingBuf {
    pub fn new(size: usize) -> Result<Self> {
        let memory = Memory::new(size)?;
        
        let metadata_ptr = memory.metadata_ptr().as_ptr() as *mut Metadata;
        unsafe {
            metadata_ptr.write(Metadata::new());
        }
        
        Ok(RingBuf { memory })
    }
    
    fn metadata(&self) -> &Metadata {
        unsafe { &*(self.memory.metadata_ptr().as_ptr() as *const Metadata) }
    }
    
    pub fn data_size(&self) -> usize {
        self.memory.data_size()
    }
    
    pub fn size_mask(&self) -> u64 {
        (self.memory.data_size() - 1) as u64
    }
    
    pub fn consumer_pos(&self) -> u64 {
        self.metadata().consumer.load(Ordering::Acquire)
    }
    
    pub fn producer_pos(&self) -> u64 {
        self.metadata().producer.load(Ordering::Acquire)
    }
    
    pub fn dropped(&self) -> u64 {
        self.metadata().dropped.load(Ordering::Relaxed)
    }
    
    pub fn data_ptr(&self) -> *mut u8 {
        self.memory.data_ptr().as_ptr()
    }
    
    pub fn advance_producer(&self, amount: u64) {
        self.metadata().producer.fetch_add(amount, Ordering::Release);
    }
    
    pub fn advance_consumer(&self, amount: u64) {
        self.metadata().consumer.fetch_add(amount, Ordering::Release);
    }
    
    pub fn increment_dropped(&self) {
        self.metadata().dropped.fetch_add(1, Ordering::Release);
    }
}

unsafe impl Send for RingBuf {}
unsafe impl Sync for RingBuf {}

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
        
        assert_eq!(ringbuf.data_size(), size - page_size);
        assert_eq!(ringbuf.consumer_pos(), 0);
        assert_eq!(ringbuf.producer_pos(), 0);
        assert_eq!(ringbuf.dropped(), 0);
        assert_eq!(ringbuf.size_mask(), (size - page_size - 1) as u64);
        
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