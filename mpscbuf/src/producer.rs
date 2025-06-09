use crate::{
    consumer::round_up_to_8, sync::notification::Notification, MpscBufError, RecordHeader, RingBuf,
    HEADER_SIZE,
};
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeupStrategy {
    // SelfPacing is experimental and runs into bugs where consumer misses wakeups.
    SelfPacing,
    Forced,
}

pub struct Producer {
    ringbuf: RingBuf,
    notification: Notification,
    wakeup_strategy: WakeupStrategy,
}

unsafe impl Sync for Producer {}

impl Producer {
    pub fn new(ringbuf: RingBuf, notification: Notification) -> Self {
        Self::with_wakeup_strategy(ringbuf, notification, WakeupStrategy::SelfPacing)
    }

    pub fn with_wakeup_strategy(
        ringbuf: RingBuf,
        notification: Notification,
        wakeup_strategy: WakeupStrategy,
    ) -> Self {
        Producer {
            ringbuf,
            notification,
            wakeup_strategy,
        }
    }

    pub fn reserve(&self, size: usize) -> Result<ReservedBuffer, MpscBufError> {
        let _guard = self.ringbuf.metadata().spinlock.lock();
        let total_size = round_up_to_8(size + HEADER_SIZE);

        let consumer_pos = self.ringbuf.consumer_pos();
        let producer_pos = self.ringbuf.producer_pos();
        let new_prod_pos = producer_pos + total_size as u64;
        if new_prod_pos - consumer_pos > self.ringbuf.size_mask() {
            return Err(MpscBufError::InsufficientSpace(
                new_prod_pos,
                consumer_pos,
                self.ringbuf.size_mask(),
            ));
        }

        let offset = producer_pos & self.ringbuf.size_mask();
        let ptr = unsafe { self.ringbuf.data_ptr().add(offset as usize) };

        unsafe {
            let header = RecordHeader::new(size as u32);
            (ptr as *mut RecordHeader).write(header);
        }
        let data_ptr = unsafe { ptr.add(HEADER_SIZE) };
        self.ringbuf.advance_producer(new_prod_pos);
        Ok(ReservedBuffer {
            data: unsafe { std::slice::from_raw_parts_mut(data_ptr, size) },
            header: unsafe { &mut *(ptr as *mut RecordHeader) },
            notification: &self.notification,
            ringbuf: &self.ringbuf,
            record_pos: offset,
            wakeup_strategy: self.wakeup_strategy,
        })
    }
}

pub struct ReservedBuffer<'a> {
    data: &'a mut [u8],
    header: &'a mut RecordHeader,
    notification: &'a Notification,
    ringbuf: &'a RingBuf,
    record_pos: u64,
    wakeup_strategy: WakeupStrategy,
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

        match self.wakeup_strategy {
            WakeupStrategy::Forced => {
                let _ = self.notification.notify();
            }
            WakeupStrategy::SelfPacing => {
                let consumer_pos = self.ringbuf.consumer_pos();
                let mask = self.ringbuf.size_mask();
                let consumer_offset = consumer_pos & mask;
                if consumer_offset == self.record_pos {
                    let _ = self.notification.notify();
                }
            }
        }
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
        let producer =
            Producer::with_wakeup_strategy(ringbuf2, notification2, WakeupStrategy::Forced);
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
    fn test_wrap_around_with_small_buffer() -> Result<(), MpscBufError> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 2;
        let ringbuf = RingBuf::new(size)?;
        let notification = Notification::new()?;

        let ringbuf_consumer = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size)?;
        let notification_consumer =
            unsafe { Notification::from_owned_fd(notification.fd().try_clone_to_owned().unwrap()) };

        let consumer = crate::Consumer::new(ringbuf_consumer, notification_consumer);
        let producer =
            Producer::with_wakeup_strategy(ringbuf, notification, WakeupStrategy::Forced);

        let _data_size = size - page_size;
        let message_size = 256;
        let num_messages = 50;

        let producer_handle = thread::spawn(move || {
            for i in 0..num_messages {
                let mut data = vec![0u8; message_size];
                for (j, byte) in data.iter_mut().enumerate() {
                    *byte = ((i * 256 + j) % 256) as u8;
                }

                loop {
                    match producer.reserve(message_size) {
                        Ok(mut reserved) => {
                            reserved.copy_from_slice(&data);
                            break;
                        }
                        Err(MpscBufError::InsufficientSpace(_, _, _)) => {
                            thread::sleep(Duration::from_micros(100));
                        }
                        Err(e) => panic!("Unexpected error: {:?}", e),
                    }
                }
            }
        });

        let consumer_handle = thread::spawn(move || {
            let mut received_messages = Vec::new();
            let mut count = 0;

            for record_result in consumer.blocking_iter() {
                match record_result {
                    Ok(record) => {
                        let data = record.as_slice().to_vec();
                        received_messages.push(data);
                        count += 1;
                        if count >= num_messages {
                            break;
                        }
                    }
                    Err(e) => panic!("Consumer error: {:?}", e),
                }
            }
            received_messages
        });

        producer_handle.join().expect("Producer thread panicked");
        let received_messages = consumer_handle.join().expect("Consumer thread panicked");

        assert_eq!(received_messages.len(), num_messages);

        for (i, message) in received_messages.iter().enumerate() {
            assert_eq!(message.len(), message_size);
            for (j, &byte) in message.iter().enumerate() {
                let expected = ((i * 256 + j) % 256) as u8;
                assert_eq!(byte, expected, "Mismatch at message {} byte {}", i, j);
            }
        }

        Ok(())
    }

    #[rstest]
    fn test_simple_multi_threaded() -> Result<(), MpscBufError> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 4;
        let ringbuf = RingBuf::new(size)?;
        let notification = Notification::new()?;

        let ringbuf_consumer = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size)?;
        let notification_consumer =
            unsafe { Notification::from_owned_fd(notification.fd().try_clone_to_owned().unwrap()) };

        let consumer = crate::Consumer::new(ringbuf_consumer, notification_consumer);
        let producer =
            Producer::with_wakeup_strategy(ringbuf, notification, WakeupStrategy::Forced);

        let num_messages = 10;
        let producer_handle = thread::spawn(move || {
            for i in 0..num_messages {
                let data = format!("msg{}", i);
                let mut reserved = producer.reserve(data.len()).unwrap();
                reserved.copy_from_slice(data.as_bytes());
                thread::sleep(Duration::from_micros(100));
            }
        });

        let consumer_handle = thread::spawn(move || {
            let mut count = 0;
            for record_result in consumer.blocking_iter() {
                match record_result {
                    Ok(_record) => {
                        count += 1;
                        if count >= num_messages {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            count
        });

        producer_handle.join().expect("Producer thread panicked");
        let received_count = consumer_handle.join().expect("Consumer thread panicked");

        assert_eq!(received_count, num_messages);
        Ok(())
    }

    #[rstest]
    fn test_two_producers() -> Result<(), MpscBufError> {
        let size = 2 * 1024 * 1024;
        let ringbuf = RingBuf::new(size)?;
        let notification = Notification::new()?;

        let ringbuf_clone1 = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size)?;
        let ringbuf_clone2 = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size)?;
        let notification1 =
            unsafe { Notification::from_owned_fd(notification.fd().try_clone_to_owned().unwrap()) };
        let notification2 =
            unsafe { Notification::from_owned_fd(notification.fd().try_clone_to_owned().unwrap()) };

        let consumer = crate::Consumer::new(ringbuf, notification);
        let producer1 = Producer::with_wakeup_strategy(
            ringbuf_clone1,
            notification1,
            WakeupStrategy::SelfPacing,
        );
        let producer2 = Producer::with_wakeup_strategy(
            ringbuf_clone2,
            notification2,
            WakeupStrategy::SelfPacing,
        );

        let num_messages = 10000;
        let handle1 = thread::spawn(move || {
            for _ in 0..num_messages / 2 {
                let data = "aaaaaaaaaaaaaaaaaa".to_string();
                let mut reserved = producer1.reserve(data.len()).unwrap();
                reserved.copy_from_slice(data.as_bytes());
                thread::sleep(Duration::from_micros(100));
            }
        });

        let handle2 = thread::spawn(move || {
            for _ in 0..num_messages / 2 {
                let data = "aaaaaaaaaaaaaaaaaa".to_string();
                let mut reserved = producer2.reserve(data.len()).unwrap();
                reserved.copy_from_slice(data.as_bytes());
                thread::sleep(Duration::from_micros(100));
            }
        });

        let consumer_handle = thread::spawn(move || {
            let mut count = 0;
            for record_result in consumer.blocking_iter() {
                match record_result {
                    Ok(_record) => {
                        count += 1;
                        if count >= num_messages {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            count
        });

        handle1.join().expect("Producer 1 panicked");
        handle2.join().expect("Producer 2 panicked");
        let received_count = consumer_handle.join().expect("Consumer thread panicked");

        assert_eq!(received_count, num_messages);
        Ok(())
    }
}
