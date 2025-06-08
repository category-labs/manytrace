use crate::memory::Memory;
use crossbeam::utils::CachePadded;
use eyre::Result;
use std::sync::atomic::{AtomicU64, Ordering};

#[repr(C)]
struct Metadata {
    spinlock: CachePadded<AtomicU64>,
    producer: AtomicU64,
    consumer: AtomicU64,
    dropped: AtomicU64,
}

impl Metadata {
    fn new() -> Self {
        Metadata {
            spinlock: CachePadded::new(AtomicU64::new(0)),
            producer: AtomicU64::new(0),
            consumer: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }
}

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
}

unsafe impl Send for RingBuf {}
unsafe impl Sync for RingBuf {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ringbuf_creation() -> Result<()> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 4;
        let ringbuf = RingBuf::new(size)?;
        
        assert_eq!(ringbuf.data_size(), size - page_size);
        assert_eq!(ringbuf.consumer_pos(), 0);
        assert_eq!(ringbuf.producer_pos(), 0);
        assert_eq!(ringbuf.dropped(), 0);
        assert_eq!(ringbuf.size_mask(), (size - page_size - 1) as u64);
        
        Ok(())
    }
}