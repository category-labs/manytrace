use crate::ringbuf::RingBuf;

#[repr(C)]
struct RecordHeader {
    len: u32,
    flags: u32,
}

const BUSY_FLAG: u32 = 1 << 31;
const DISCARD_FLAG: u32 = 1 << 30;
const HEADER_SIZE: usize = std::mem::size_of::<RecordHeader>();

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
}

impl Consumer {
    pub fn new(ringbuf: RingBuf) -> Self {
        Consumer { ringbuf }
    }
    
    pub fn iter(&self) -> ConsumerIter {
        ConsumerIter::new(&self.ringbuf)
    }
    
    pub fn available_records(&self) -> u64 {
        let prod_pos = self.ringbuf.producer_pos();
        let cons_pos = self.ringbuf.consumer_pos();
        
        if prod_pos >= cons_pos {
            prod_pos - cons_pos
        } else {
            0
        }
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
            
            if cons_pos >= prod_pos {
                return None;
            }
            
            let data_ptr = self.ringbuf.data_ptr();
            let mask = self.ringbuf.size_mask();
            let offset = (cons_pos & mask) as usize;
            
            let header_ptr = unsafe { data_ptr.add(offset) as *const RecordHeader };
            let header = unsafe { header_ptr.read_volatile() };
            
            if header.flags & BUSY_FLAG != 0 {
                return None;
            }
            
            let record_len = header.len as usize;
            let total_len = round_up_to_8(HEADER_SIZE + record_len) as u64;
            
            if header.flags & DISCARD_FLAG == 0 {
                let data_offset = offset + HEADER_SIZE;
                let record_data = unsafe {
                    std::slice::from_raw_parts(
                        data_ptr.add(data_offset & mask as usize),
                        record_len
                    )
                };
                
                return Some(Record::new(
                    self.ringbuf,
                    record_data,
                    total_len,
                ));
            } else {
                self.ringbuf.advance_consumer(total_len);
            }
        }
    }
}

fn round_up_to_8(len: usize) -> usize {
    (len + 7) & !7
}