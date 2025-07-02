use crate::{
    common::{likely, unlikely, RecordHeader, BUSY_FLAG, DISCARD_FLAG, HEADER_SIZE},
    ringbuf::RingBuf,
    sync::notification::Notification,
    MpscBufError,
};

pub struct Record<'a> {
    ringbuf: &'a RingBuf,
    data: &'a [u8],
    total_len: u64,
}

impl<'a> Record<'a> {
    #[inline(always)]
    fn new(ringbuf: &'a RingBuf, data: &'a [u8], total_len: u64) -> Self {
        Record {
            ringbuf,
            data,
            total_len,
        }
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[u8] {
        self.data
    }
}

impl<'a> Drop for Record<'a> {
    #[inline(always)]
    fn drop(&mut self) {
        self.ringbuf.advance_consumer(self.total_len);
    }
}

pub struct Consumer {
    ringbuf: RingBuf,
    notification: Notification,
}

impl Consumer {
    /// Create a new consumer with the specified buffer size.
    ///
    /// This is the recommended way to create a consumer. The buffer size should be
    /// a power of two and at least one page size (typically 4096 bytes).
    pub fn new(data_size: usize) -> Result<Self, MpscBufError> {
        let ringbuf = RingBuf::new(data_size)?;
        let notification = Notification::new()?;
        Ok(Consumer::from_parts(ringbuf, notification))
    }

    pub(crate) fn from_parts(ringbuf: RingBuf, notification: Notification) -> Self {
        Consumer {
            ringbuf,
            notification,
        }
    }

    pub fn notification_fd(&self) -> std::os::fd::BorrowedFd {
        self.notification.fd()
    }

    pub fn memory_fd(&self) -> std::os::fd::BorrowedFd {
        self.ringbuf.memory_fd()
    }

    pub fn memory_size(&self) -> usize {
        self.data_size() + unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    pub fn data_size(&self) -> usize {
        self.ringbuf.data_size()
    }

    #[inline(always)]
    pub fn consume(&mut self) -> Option<Record<'_>> {
        loop {
            let cons_pos = self.ringbuf.consumer_pos();
            let prod_pos = self.ringbuf.producer_pos();

            crate::mpsc_trace!(
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

            crate::mpsc_trace!(
                cons_pos = cons_pos,
                offset = offset,
                record_len = record_len_u32,
                flags = flags,
                busy = flags & BUSY_FLAG,
                discard = flags & DISCARD_FLAG,
                "consumer record header read"
            );

            if unlikely(flags & BUSY_FLAG != 0) {
                return None;
            }

            let record_len = record_len_u32 as usize;
            let total_len = round_up_to_8(HEADER_SIZE + record_len) as u64;

            if likely(flags & DISCARD_FLAG == 0) {
                let data_offset = offset + HEADER_SIZE;
                let record_data =
                    unsafe { std::slice::from_raw_parts(data_ptr.add(data_offset), record_len) };

                crate::mpsc_trace!(
                    cons_pos = cons_pos,
                    new_cons_pos = cons_pos + total_len,
                    record_len = record_len,
                    "consumer record consumed"
                );

                return Some(Record::new(
                    &self.ringbuf,
                    record_data,
                    cons_pos + total_len,
                ));
            } else {
                crate::mpsc_trace!(
                    cons_pos = cons_pos,
                    new_cons_pos = cons_pos + total_len,
                    "consumer record discarded"
                );

                self.ringbuf.advance_consumer(cons_pos + total_len);
            }
        }
    }

    pub fn wait(&mut self) -> Result<(), MpscBufError> {
        crate::mpsc_trace!("consumer wait start");

        let result = self.notification.wait();
        #[cfg_attr(not(feature = "trace"), allow(unused_variables))]
        let success = result.is_ok();

        crate::mpsc_trace!(success = success, "consumer wait end");

        result
    }

    pub fn available_records(&self) -> u64 {
        let prod_pos = self.ringbuf.producer_pos();
        let cons_pos = self.ringbuf.consumer_pos();

        prod_pos.saturating_sub(cons_pos)
    }

    /// Get the total number of dropped records.
    ///
    /// This counter tracks records that were intentionally dropped by producers
    /// due to insufficient space or other conditions.
    pub fn dropped(&self) -> u64 {
        self.ringbuf.dropped()
    }
}

#[inline(always)]
pub(crate) fn round_up_to_8(len: usize) -> usize {
    len.div_ceil(8) * 8
}
