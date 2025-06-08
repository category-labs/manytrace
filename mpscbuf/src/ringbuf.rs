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
    
    pub fn metadata(&self) -> &Metadata {
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

    #[rstest]
    fn test_spinlock_basic(ringbuf: RingBuf) {
        let metadata = ringbuf.metadata();
        
        {
            let _guard = metadata.spinlock.try_lock();
            assert!(_guard.is_some());
        }
        
        {
            let _guard = metadata.spinlock.lock();
        }
    }

    #[rstest]
    fn test_spinlock_contention() {
        use std::sync::Arc;
        use std::thread;
        
        let ringbuf = Arc::new({
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
            let size = page_size * 2;
            RingBuf::new(size).unwrap()
        });
        
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let ringbuf = Arc::clone(&ringbuf);
                let counter = Arc::clone(&counter);
                
                thread::spawn(move || {
                    for _ in 0..100 {
                        let _guard = ringbuf.metadata().spinlock.lock();
                        
                        // Critical section
                        let old = counter.load(std::sync::atomic::Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_nanos(1));
                        counter.store(old + 1, std::sync::atomic::Ordering::Relaxed);
                    }
                })
            })
            .collect();
        
        for handle in handles {
            handle.join().unwrap();
        }
        
        assert_eq!(counter.load(std::sync::atomic::Ordering::Relaxed), 400);
    }
}