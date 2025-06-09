pub mod consumer;
pub mod error;
pub mod memory;
pub mod producer;
pub mod ringbuf;
pub mod sync;

use crossbeam::utils::CachePadded;
use sync::{AtomicU64, Ordering, Spinlock};

#[repr(C)]
pub(crate) struct Metadata {
    pub(crate) spinlock: CachePadded<Spinlock<()>>,
    pub(crate) producer: AtomicU64,
    pub(crate) consumer: AtomicU64,
    pub(crate) dropped: AtomicU64,
}

impl Metadata {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Metadata {
            spinlock: CachePadded::new(Spinlock::new(())),
            producer: AtomicU64::new(0),
            consumer: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }
}

#[repr(C)]
pub(crate) struct RecordHeader {
    header: AtomicU64,
}

pub(crate) const BUSY_FLAG: u32 = 1 << 31;
pub(crate) const DISCARD_FLAG: u32 = 1 << 30;
pub(crate) const HEADER_SIZE: usize = std::mem::size_of::<RecordHeader>();

impl RecordHeader {
    pub(crate) fn new(len: u32) -> Self {
        let header_value = (len as u64) | ((BUSY_FLAG as u64) << 32);
        RecordHeader {
            header: AtomicU64::new(header_value),
        }
    }

    pub(crate) fn discard(&self) {
        let current = self.header.load(Ordering::Relaxed);
        let len = current as u32;
        let new_value = (len as u64) | ((DISCARD_FLAG as u64) << 32);
        self.header.store(new_value, Ordering::Release);
    }

    pub(crate) fn commit(&self) {
        let current = self.header.load(Ordering::Relaxed);
        let len = current as u32;
        let new_value = len as u64;
        self.header.store(new_value, Ordering::Release);
    }

    pub(crate) fn is_discarded(&self) -> bool {
        let current = self.header.load(Ordering::Relaxed);
        let flags = (current >> 32) as u32;
        flags & DISCARD_FLAG != 0
    }

    pub(crate) fn len_and_flags(&self) -> (u32, u32) {
        let current = self.header.load(Ordering::Acquire);
        let len = current as u32;
        let flags = (current >> 32) as u32;
        (len, flags)
    }
}

// Public API - these are what users should use
pub use consumer::{Consumer, Record};
pub use error::MpscBufError;
pub use producer::{Producer, ReservedBuffer, WakeupStrategy};
pub use ringbuf::RingBuf;
pub use sync::notification::Notification;

// Re-export for convenience
pub use eyre::Result;

// Internal types - still exposed for now but marked as implementation details
pub use consumer::ConsumerIter;

/// Create a new consumer with the specified buffer size.
///
/// This is the recommended way to create a consumer. The buffer size should be
/// a power of two and at least twice the page size.
///
/// # Example
/// ```rust
/// use mpscbuf::create_consumer;
///
/// let consumer = create_consumer(1024 * 1024)?;  // 1MB buffer
/// # Ok::<(), mpscbuf::MpscBufError>(())
/// ```
pub fn create_consumer(data_size: usize) -> Result<Consumer, MpscBufError> {
    let ringbuf = RingBuf::new(data_size)?;
    let notification = Notification::new()?;
    Ok(Consumer::new(ringbuf, notification))
}

/// Create a producer from file descriptors (typically for cross-process usage).
///
/// This function validates that the file descriptors are still open and usable
/// before creating the producer.
///
/// # Arguments
/// * `memory_fd` - File descriptor for the shared memory
/// * `notification_fd` - File descriptor for the notification mechanism  
/// * `data_size` - Size of the ring buffer data area
/// * `wakeup_strategy` - Strategy for notifying the consumer
///
/// # Errors
/// Returns an error if either file descriptor is closed or invalid.
///
/// # Example
/// ```rust,no_run
/// use mpscbuf::{create_producer, WakeupStrategy};
/// use std::os::fd::OwnedFd;
///
/// # let memory_fd: OwnedFd = unimplemented!();
/// # let notification_fd: OwnedFd = unimplemented!();
/// let producer = create_producer(memory_fd, notification_fd, 1024 * 1024, WakeupStrategy::Forced)?;
/// # Ok::<(), mpscbuf::MpscBufError>(())
/// ```
pub fn create_producer(
    memory_fd: std::os::fd::OwnedFd,
    notification_fd: std::os::fd::OwnedFd,
    data_size: usize,
    wakeup_strategy: WakeupStrategy,
) -> Result<Producer, MpscBufError> {
    use std::os::fd::AsFd;

    // Validate that the file descriptors are still open by checking if we can get metadata
    // This will fail if the FD is closed or invalid
    nix::sys::stat::fstat(&memory_fd)
        .map_err(|errno| MpscBufError::IoError(std::io::Error::from_raw_os_error(errno as i32)))?;
    nix::sys::stat::fstat(&notification_fd)
        .map_err(|errno| MpscBufError::IoError(std::io::Error::from_raw_os_error(errno as i32)))?;

    let ringbuf = RingBuf::from_fd(memory_fd, data_size)?;
    let notification = unsafe { Notification::from_owned_fd(notification_fd) };

    Ok(Producer::with_wakeup_strategy(
        ringbuf,
        notification,
        wakeup_strategy,
    ))
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use loom::{model::Builder, thread};

    #[test]
    fn test_four_producers() {
        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(3);
        }

        builder.check(|| {
            let page_size = 4096;
            let size = page_size * 8;

            let ringbuf = RingBuf::new(size).unwrap();
            let ringbuf_clone = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size).unwrap();
            let notification = Notification::new().unwrap();

            let consumer = Consumer::new(ringbuf, notification.clone());

            let num_producers = 3;
            let msgs_per_producer = 2;

            let mut handles = vec![];

            for producer_id in 0..num_producers {
                let ringbuf_clone =
                    RingBuf::from_fd(ringbuf_clone.clone_fd().unwrap(), size).unwrap();
                let notification_clone = notification.clone();
                let producer = Producer::with_wakeup_strategy(
                    ringbuf_clone,
                    notification_clone,
                    WakeupStrategy::Forced,
                );

                let handle = thread::spawn(move || {
                    for i in 0..msgs_per_producer {
                        let data = format!("p{}_{}", producer_id, i);
                        if let Ok(mut reserved) = producer.reserve(data.len()) {
                            reserved.copy_from_slice(data.as_bytes());
                        }
                    }
                });

                handles.push(handle);
            }

            let mut count = 0;
            let mut producer_counts = vec![0; num_producers];
            for record in consumer.iter() {
                let data = std::str::from_utf8(record.as_slice()).unwrap();
                for (producer_id, count) in producer_counts.iter_mut().enumerate() {
                    if data.starts_with(&format!("p{}_", producer_id)) {
                        *count += 1;
                        break;
                    }
                }
                count += 1;
            }

            for handle in handles {
                handle.join().unwrap();
            }
            for record in consumer.iter() {
                let data = std::str::from_utf8(record.as_slice()).unwrap();
                for (producer_id, count) in producer_counts.iter_mut().enumerate() {
                    if data.starts_with(&format!("p{}_", producer_id)) {
                        *count += 1;
                        break;
                    }
                }
                count += 1;
            }

            assert_eq!(count, msgs_per_producer * num_producers);
            for (producer_id, count) in producer_counts.iter().enumerate() {
                assert_eq!(
                    *count, msgs_per_producer,
                    "Producer {} sent {} messages, expected {}",
                    producer_id, *count, msgs_per_producer
                );
            }
        });
    }

    #[test]
    fn test_single_producer_single_consumer() {
        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(3);
        }

        builder.check(|| {
            let page_size = 4096;
            let size = page_size * 4;

            let ringbuf = RingBuf::new(size).unwrap();
            let ringbuf_clone = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size).unwrap();
            let notification = Notification::new().unwrap();

            let consumer = Consumer::new(ringbuf, notification.clone());
            let producer =
                Producer::with_wakeup_strategy(ringbuf_clone, notification, WakeupStrategy::Forced);

            let num_messages = 3;

            let producer_handle = thread::spawn(move || {
                for i in 0..num_messages {
                    let data = format!("message_{}", i);
                    if let Ok(mut reserved) = producer.reserve(data.len()) {
                        reserved.copy_from_slice(data.as_bytes());
                    }
                }
            });

            let mut received = vec![];
            while received.len() < num_messages {
                for record in consumer.iter() {
                    let data = std::str::from_utf8(record.as_slice()).unwrap();
                    received.push(data.to_string());
                    if received.len() >= num_messages {
                        break;
                    }
                }
                if received.len() < num_messages {
                    consumer.wait().unwrap();
                }
            }

            producer_handle.join().unwrap();

            assert_eq!(received.len(), num_messages);
            for i in 0..num_messages {
                assert!(received.contains(&format!("message_{}", i)));
            }
        });
    }
}
