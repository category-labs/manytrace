use crate::{MpscBufError, RecordHeader, RingBuf, HEADER_SIZE};
use eyre::{ensure, Result};
use std::ops::{Deref, DerefMut};

pub struct Producer {
    ringbuf: RingBuf,
}

impl Producer {
    pub fn new(ringbuf: RingBuf) -> Self {
        Producer { ringbuf }
    }

    pub fn reserve(&self, size: usize) -> Result<ReservedBuffer> {
        let total_size = (size + HEADER_SIZE + 7) & !7;
        let producer_pos = self.ringbuf.producer_pos();
        let consumer_pos = self.ringbuf.consumer_pos();
        let data_size = self.ringbuf.data_size() as u64;

        ensure!(
            producer_pos - consumer_pos + total_size as u64 <= data_size,
            MpscBufError::InsufficientSpace
        );

        let offset = producer_pos & self.ringbuf.size_mask();
        let ptr = unsafe { self.ringbuf.data_ptr().add(offset as usize) };

        unsafe {
            let header = RecordHeader::new(size as u32);
            (ptr as *mut RecordHeader).write(header);
        }

        self.ringbuf.advance_producer(total_size as u64);

        let data_ptr = unsafe { ptr.add(HEADER_SIZE) };
        let data_slice = unsafe { std::slice::from_raw_parts_mut(data_ptr, size) };

        let header_ref = unsafe { &mut *(ptr as *mut RecordHeader) };

        Ok(ReservedBuffer {
            data: data_slice,
            header: header_ref,
        })
    }
}

pub struct ReservedBuffer<'a> {
    data: &'a mut [u8],
    header: &'a mut RecordHeader,
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
        // SAFETY nothing is owned by ReservedBuffer
        std::mem::forget(self);
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

impl<'a> Drop for ReservedBuffer<'a> {
    fn drop(&mut self) {
        self.header.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::*;

    #[fixture]
    fn producer() -> Producer {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let size = page_size * 2;
        let ringbuf = RingBuf::new(size).unwrap();
        Producer::new(ringbuf)
    }

    #[rstest]
    fn test_reserve_and_commit(producer: Producer) -> Result<()> {
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
    fn test_explicit_discard(producer: Producer) -> Result<()> {
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
    fn test_single_writer_reader(producer: Producer, #[case] messages: &[&[u8]]) -> Result<()> {
        for msg in messages {
            let mut reserved = producer.reserve(msg.len())?;
            // Can now use reserved directly as a slice thanks to DerefMut
            reserved[..msg.len()].copy_from_slice(msg);
        }

        let consumer = crate::Consumer::new(producer.ringbuf);
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
    fn test_various_sizes(producer: Producer, #[case] size: usize) -> Result<()> {
        let initial_pos = producer.ringbuf.producer_pos();

        {
            let mut reserved = producer.reserve(size)?;
            // Can now use reserved directly as a slice thanks to DerefMut
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
    fn test_insufficient_space(producer: Producer) -> Result<()> {
        let data_size = producer.ringbuf.data_size();
        let large_size = data_size / 2;
        
        // First reservation should succeed
        let _reserved1 = producer.reserve(large_size)?;
        
        // Second reservation should fail due to insufficient space
        assert!(producer.reserve(large_size).is_err());
        
        Ok(())
    }
}
