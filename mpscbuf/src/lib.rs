pub mod consumer;
pub mod error;
pub mod memory;
pub mod producer;
pub mod ringbuf;

use crossbeam::utils::CachePadded;
use spinning_top::Spinlock;
use std::sync::atomic::{AtomicU64, Ordering};

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
        let current = self.header.load(Ordering::Acquire);
        let len = current as u32;
        let new_flags = DISCARD_FLAG;
        let new_value = (len as u64) | ((new_flags as u64) << 32);
        self.header.store(new_value, Ordering::Release);
    }

    pub fn commit(&self) {
        let current = self.header.load(Ordering::Acquire);
        let len = current as u32;
        let new_value = len as u64;
        self.header.store(new_value, Ordering::Release);
    }

    pub fn len(&self) -> u32 {
        let current = self.header.load(Ordering::Acquire);
        current as u32
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn flags(&self) -> u32 {
        let current = self.header.load(Ordering::Acquire);
        (current >> 32) as u32
    }
}

pub use consumer::{Consumer, ConsumerIter, Record};
pub use error::MpscBufError;
pub use memory::Memory;
pub use producer::{Producer, ReservedBuffer};
pub use ringbuf::RingBuf;

pub use eyre::{Result, WrapErr};
