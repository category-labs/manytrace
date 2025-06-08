use crate::{sync::notification::Notification, MpscBufError, RecordHeader, RingBuf, HEADER_SIZE};
use std::ops::{Deref, DerefMut};

pub struct Producer {
    ringbuf: RingBuf,
    notification: Notification,
}

unsafe impl Sync for Producer {}

impl Producer {
    pub fn new(ringbuf: RingBuf, notification: Notification) -> Self {
        Producer {
            ringbuf,
            notification,
        }
    }

    pub fn reserve(&self, size: usize) -> Result<ReservedBuffer, MpscBufError> {
        let _guard = self.ringbuf.metadata().spinlock.lock();

        let total_size = (size + HEADER_SIZE + 7) & !7;
        let consumer_pos = self.ringbuf.consumer_pos();
        let producer_pos = self.ringbuf.producer_pos();
        let data_size = self.ringbuf.data_size() as u64;
        if producer_pos - consumer_pos + total_size as u64 > data_size {
            return Err(MpscBufError::InsufficientSpace);
        }

        let offset = producer_pos & self.ringbuf.size_mask();
        let ptr = unsafe { self.ringbuf.data_ptr().add(offset as usize) };

        unsafe {
            let header = RecordHeader::new(size as u32);
            (ptr as *mut RecordHeader).write(header);
        }
        self.ringbuf
            .advance_producer(producer_pos + total_size as u64);

        let data_ptr = unsafe { ptr.add(HEADER_SIZE) };
        Ok(ReservedBuffer {
            data: unsafe { std::slice::from_raw_parts_mut(data_ptr, size) },
            header: unsafe { &mut *(ptr as *mut RecordHeader) },
            notification: &self.notification,
            // ringbuf: &self.ringbuf,
            // record_pos: offset,
        })
    }
}

pub struct ReservedBuffer<'a> {
    data: &'a mut [u8],
    header: &'a mut RecordHeader,
    notification: &'a Notification,
    // ringbuf: &'a RingBuf,
    // record_pos: u64,
}

impl<'a> ReservedBuffer<'a> {
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.data
    }

    pub fn size(&self) -> usize {
        self.data.len()
    }

    pub fn discard(self) {
        self.header.discard();
    }
}

impl<'a> Drop for ReservedBuffer<'a> {
    fn drop(&mut self) {
        if !self.header.is_discarded() {
            self.header.commit();
        }
        // let consumer_pos = self.ringbuf.consumer_pos();
        // let mask = self.ringbuf.size_mask();
        // let consumer_offset = consumer_pos & mask;
        // if consumer_offset == self.record_pos {
        //     let _ = self.notification.notify();
        // }
        let _ = self.notification.notify();
    }
}

impl<'a> Deref for ReservedBuffer<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.data
    }
}

impl<'a> DerefMut for ReservedBuffer<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::*;
    use std::thread;
    use std::time::Duration;

    #[fixture]
    fn producer_and_consumer() -> (Producer, crate::Consumer) {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 2;
        let ringbuf1 = RingBuf::new(size).unwrap();
        let ringbuf2 = RingBuf::from_fd(ringbuf1.clone_fd().unwrap(), size).unwrap();
        let notification1 = Notification::new().unwrap();
        let notification2 = unsafe {
            Notification::from_owned_fd(notification1.fd().try_clone_to_owned().unwrap())
        };
        let consumer = crate::Consumer::new(ringbuf1, notification1);
        let producer = Producer::new(ringbuf2, notification2);
        (producer, consumer)
    }

    #[fixture]
    fn producer() -> Producer {
        producer_and_consumer().0
    }

    #[rstest]
    fn test_reserve_and_commit(producer: Producer) -> Result<(), MpscBufError> {
        let data = b"hello world!";
        let data_len = data.len();

        let initial_pos = producer.ringbuf.producer_pos();

        {
            let mut reserved = producer.reserve(data_len)?;
            // Can now use reserved directly as a slice thanks to DerefMut
            reserved[..data_len].copy_from_slice(data);
        }

        let total_size = (data_len + HEADER_SIZE + 7) & !7;
        assert_eq!(
            producer.ringbuf.producer_pos(),
            initial_pos + total_size as u64
        );
        assert_eq!(producer.ringbuf.consumer_pos(), 0);

        Ok(())
    }

    #[rstest]
    fn test_explicit_discard(producer: Producer) -> Result<(), MpscBufError> {
        let initial_pos = producer.ringbuf.producer_pos();
        {
            let reserved = producer.reserve(16)?;
            reserved.discard();
        }
        let total_size = (16 + HEADER_SIZE + 7) & !7;
        assert_eq!(
            producer.ringbuf.producer_pos(),
            initial_pos + total_size as u64
        );
        Ok(())
    }

    #[rstest]
    #[case::small_messages(&[&b"hello"[..], &b"world"[..]])]
    #[case::mixed_messages(&[&b"first message"[..], &b"second message"[..], &b"third message"[..]])]
    fn test_single_writer_reader(#[case] messages: &[&[u8]]) -> Result<(), MpscBufError> {
        let (producer, consumer) = producer_and_consumer();

        for msg in messages {
            let mut reserved = producer.reserve(msg.len())?;
            // Can now use reserved directly as a slice thanks to DerefMut
            reserved[..msg.len()].copy_from_slice(msg);
        }
        let mut count = 0;

        for (i, record) in consumer.iter().enumerate() {
            let data = record.as_slice();
            let expected_len = messages[i].len();
            assert_eq!(&data[..expected_len], messages[i]);
            count += 1;
        }

        assert_eq!(count, messages.len());

        Ok(())
    }

    #[rstest]
    #[case(8)]
    #[case(16)]
    #[case(32)]
    #[case(64)]
    #[case(128)]
    fn test_various_sizes(producer: Producer, #[case] size: usize) -> Result<(), MpscBufError> {
        let initial_pos = producer.ringbuf.producer_pos();

        {
            let mut reserved = producer.reserve(size)?;
            for (i, byte) in reserved[..size].iter_mut().enumerate() {
                *byte = (i % 256) as u8;
            }
        }

        let total_size = (size + HEADER_SIZE + 7) & !7;
        assert_eq!(
            producer.ringbuf.producer_pos(),
            initial_pos + total_size as u64
        );

        Ok(())
    }

    #[rstest]
    fn test_invalid_size_validation(producer: Producer) {
        let huge_size = producer.ringbuf.data_size() + 1;
        assert!(producer.reserve(huge_size).is_err());
    }

    #[rstest]
    fn test_insufficient_space(producer: Producer) -> Result<(), MpscBufError> {
        let data_size = producer.ringbuf.data_size();
        let large_size = data_size / 2;
        let _reserved1 = producer.reserve(large_size)?;
        assert!(producer.reserve(large_size).is_err());
        Ok(())
    }

    #[rstest]
    fn test_multi_threaded_producer_consumer() -> Result<(), MpscBufError> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 8;
        let ringbuf = RingBuf::new(size)?;
        let notification = Notification::new()?;

        let ringbuf_clone1 = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size)?;
        let ringbuf_clone2 = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size)?;
        let notification1 =
            unsafe { Notification::from_owned_fd(notification.fd().try_clone_to_owned().unwrap()) };
        let notification2 =
            unsafe { Notification::from_owned_fd(notification.fd().try_clone_to_owned().unwrap()) };

        let consumer = crate::Consumer::new(ringbuf, notification);
        let producer1 = Producer::new(ringbuf_clone1, notification1);
        let producer2 = Producer::new(ringbuf_clone2, notification2);

        let consumer_handle = thread::spawn(move || {
            let mut count = 0;
            let mut messages = Vec::new();

            for record_result in consumer.blocking_iter() {
                match record_result {
                    Ok(record) => {
                        let data = String::from_utf8_lossy(record.as_slice());
                        messages.push(data.to_string());
                        count += 1;
                        if count >= 20 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            messages
        });

        thread::sleep(Duration::from_millis(10));

        let handle1 = {
            thread::spawn(move || {
                for i in 0..10 {
                    let data = format!("message_from_thread1_{}", i);
                    let mut reserved = producer1.reserve(data.len()).unwrap();
                    reserved.copy_from_slice(data.as_bytes());
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let handle2 = {
            thread::spawn(move || {
                for i in 0..10 {
                    let data = format!("message_from_thread2_{}", i);
                    let mut reserved = producer2.reserve(data.len()).unwrap();
                    reserved.copy_from_slice(data.as_bytes());
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        handle1.join().expect("Thread 1 panicked");
        handle2.join().expect("Thread 2 panicked");

        thread::sleep(Duration::from_millis(100));

        let messages = consumer_handle.join().expect("Consumer thread panicked");

        let thread1_messages = messages.iter().filter(|m| m.contains("thread1")).count();
        let thread2_messages = messages.iter().filter(|m| m.contains("thread2")).count();

        assert!(thread1_messages > 0, "Should have messages from thread 1");
        assert!(thread2_messages > 0, "Should have messages from thread 2");
        assert_eq!(messages.len(), 20, "Should have 20 total messages");

        Ok(())
    }
}
