use crate::{
    sync::notification::Notification, MpscBufError, RecordHeader, RingBuf, BUSY_FLAG, DISCARD_FLAG,
    HEADER_SIZE,
};

#[cfg(feature = "tracing")]
use tracing::trace;

pub struct Record<'a> {
    ringbuf: &'a RingBuf,
    data: &'a [u8],
    total_len: u64,
}

impl<'a> Record<'a> {
    fn new(ringbuf: &'a RingBuf, data: &'a [u8], total_len: u64) -> Self {
        Record {
            ringbuf,
            data,
            total_len,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.data
    }
}

impl<'a> Drop for Record<'a> {
    fn drop(&mut self) {
        self.ringbuf.advance_consumer(self.total_len);
    }
}

pub struct Consumer {
    ringbuf: RingBuf,
    notification: Notification,
}

impl Consumer {
    pub fn new(ringbuf: RingBuf, notification: Notification) -> Self {
        Consumer {
            ringbuf,
            notification,
        }
    }

    pub fn notification(&self) -> &Notification {
        &self.notification
    }

    pub fn notification_fd(&self) -> std::os::fd::BorrowedFd {
        self.notification.fd()
    }

    pub fn memory_fd(&self) -> std::os::fd::BorrowedFd {
        self.ringbuf.memory_fd()
    }

    pub fn memory_size(&self) -> usize {
        self.ringbuf.data_size() + unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    pub fn iter(&self) -> ConsumerIter {
        ConsumerIter::new(&self.ringbuf)
    }

    pub fn wait(&self) -> Result<(), MpscBufError> {
        #[cfg(feature = "tracing")]
        trace!("consumer wait start");
        
        let result = self.notification.wait();
        
        #[cfg(feature = "tracing")]
        trace!(success = result.is_ok(), "consumer wait end");
        
        result
    }

    pub fn available_records(&self) -> u64 {
        let prod_pos = self.ringbuf.producer_pos();
        let cons_pos = self.ringbuf.consumer_pos();

        prod_pos.saturating_sub(cons_pos)
    }
}

impl<'a> IntoIterator for &'a Consumer {
    type Item = Record<'a>;
    type IntoIter = ConsumerIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

pub struct ConsumerIter<'a> {
    ringbuf: &'a RingBuf,
}

impl<'a> ConsumerIter<'a> {
    fn new(ringbuf: &'a RingBuf) -> Self {
        ConsumerIter { ringbuf }
    }
}

impl<'a> Iterator for ConsumerIter<'a> {
    type Item = Record<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let cons_pos = self.ringbuf.consumer_pos();
            let prod_pos = self.ringbuf.producer_pos();
            
            #[cfg(feature = "tracing")]
            trace!(
                cons_pos = cons_pos,
                prod_pos = prod_pos,
                "consumer iter next attempt"
            );
            
            if cons_pos >= prod_pos {
                return None;
            }

            let data_ptr = self.ringbuf.data_ptr();
            let mask = self.ringbuf.size_mask();
            let offset = (cons_pos & mask) as usize;

            let header_ptr = unsafe { data_ptr.add(offset) as *const RecordHeader };

            let header = unsafe { &*header_ptr };
            let (record_len_u32, flags) = header.len_and_flags();
            
            #[cfg(feature = "tracing")]
            trace!(
                cons_pos = cons_pos,
                offset = offset,
                record_len = record_len_u32,
                flags = flags,
                busy = flags & BUSY_FLAG,
                discard = flags & DISCARD_FLAG,
                "consumer record header read"
            );
            
            if flags & BUSY_FLAG != 0 {
                return None;
            }

            let record_len = record_len_u32 as usize;
            let total_len = round_up_to_8(HEADER_SIZE + record_len) as u64;

            if flags & DISCARD_FLAG == 0 {
                let data_offset = offset + HEADER_SIZE;
                let record_data =
                    unsafe { std::slice::from_raw_parts(data_ptr.add(data_offset), record_len) };
                
                #[cfg(feature = "tracing")]
                trace!(
                    cons_pos = cons_pos,
                    new_cons_pos = cons_pos + total_len,
                    record_len = record_len,
                    "consumer record consumed"
                );
                
                return Some(Record::new(self.ringbuf, record_data, cons_pos + total_len));
            } else {
                #[cfg(feature = "tracing")]
                trace!(
                    cons_pos = cons_pos,
                    new_cons_pos = cons_pos + total_len,
                    "consumer record discarded"
                );
                
                self.ringbuf.advance_consumer(cons_pos + total_len);
            }
        }
    }
}

pub fn round_up_to_8(len: usize) -> usize {
    len.div_ceil(8) * 8
}
