pub mod consumer;
pub mod error;
pub mod memory;
pub mod producer;
pub mod ringbuf;
pub mod sync;

use crossbeam::utils::CachePadded;
use sync::{AtomicU64, Ordering, Spinlock};

#[repr(C)]
pub struct Metadata {
    pub spinlock: CachePadded<Spinlock<()>>,
    pub producer: AtomicU64,
    pub consumer: AtomicU64,
    pub dropped: AtomicU64,
}

impl Metadata {
    pub fn new() -> Self {
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
pub struct RecordHeader {
    header: AtomicU64,
}

pub const BUSY_FLAG: u32 = 1 << 31;
pub const DISCARD_FLAG: u32 = 1 << 30;
pub const HEADER_SIZE: usize = std::mem::size_of::<RecordHeader>();

impl RecordHeader {
    pub fn new(len: u32) -> Self {
        let header_value = (len as u64) | ((BUSY_FLAG as u64) << 32);
        RecordHeader {
            header: AtomicU64::new(header_value),
        }
    }

    pub fn discard(&self) {
        let current = self.header.load(Ordering::Relaxed);
        let len = current as u32;
        let new_value = (len as u64) | ((DISCARD_FLAG as u64) << 32);
        self.header.store(new_value, Ordering::Release);
    }

    pub fn commit(&self) {
        let current = self.header.load(Ordering::Relaxed);
        let len = current as u32;
        let new_value = len as u64;
        self.header.store(new_value, Ordering::Release);
    }

    pub fn is_discarded(&self) -> bool {
        let current = self.header.load(Ordering::Relaxed);
        let flags = (current >> 32) as u32;
        flags & DISCARD_FLAG != 0
    }

    pub fn len_and_flags(&self) -> (u32, u32) {
        let current = self.header.load(Ordering::Acquire);
        let len = current as u32;
        let flags = (current >> 32) as u32;
        (len, flags)
    }

    pub fn write_initial(&self, len: u32) {
        let header_value = (len as u64) | ((BUSY_FLAG as u64) << 32);
        self.header.store(header_value, Ordering::Release);
    }
}

pub use consumer::{Consumer, ConsumerIter, Record};
pub use error::MpscBufError;
pub use memory::Memory;
pub use producer::{Producer, ReservedBuffer, WakeupStrategy};
pub use ringbuf::RingBuf;
pub use sync::notification::Notification;

pub use eyre::{Result, WrapErr};

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
