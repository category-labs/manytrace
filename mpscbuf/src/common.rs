// Copyright (C) 2025 Category Labs, Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Common internal types and utilities for mpscbuf.

use crate::sync::{AtomicU64, Ordering, Spinlock};
use crossbeam::utils::CachePadded;

#[inline]
#[cold]
fn cold() {}

#[allow(unused)]
#[inline(always)]
pub(crate) fn likely(b: bool) -> bool {
    if !b {
        cold();
    }
    b
}

#[inline(always)]
pub(crate) fn unlikely(b: bool) -> bool {
    if b {
        cold();
    }
    b
}

#[repr(C)]
pub(crate) struct Metadata {
    pub(crate) spinlock: CachePadded<Spinlock>,
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
            spinlock: CachePadded::new(Spinlock::new()),
            producer: AtomicU64::new(0),
            consumer: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }
}

#[repr(transparent)]
pub(crate) struct RecordHeader(AtomicU64);

pub(crate) const BUSY_FLAG: u32 = 1 << 31;
pub(crate) const DISCARD_FLAG: u32 = 1 << 30;
pub(crate) const HEADER_SIZE: usize = std::mem::size_of::<RecordHeader>();

impl RecordHeader {
    #[inline(always)]
    pub(crate) fn new(len: u32) -> Self {
        RecordHeader(AtomicU64::new((len as u64) | ((BUSY_FLAG as u64) << 32)))
    }

    #[inline(always)]
    pub(crate) fn discard(&self) {
        let current = self.0.load(Ordering::Relaxed);
        let len = current as u32;
        let new_value = (len as u64) | ((DISCARD_FLAG as u64) << 32);
        self.0.store(new_value, Ordering::Release);
    }

    #[inline(always)]
    pub(crate) fn commit(&self) {
        let current = self.0.load(Ordering::Relaxed);
        let len = current as u32;
        let new_value = len as u64;
        self.0.store(new_value, Ordering::Release);
    }

    #[inline(always)]
    pub(crate) fn is_discarded(&self) -> bool {
        let current = self.0.load(Ordering::Relaxed);
        let flags = (current >> 32) as u32;
        flags & DISCARD_FLAG != 0
    }

    #[inline(always)]
    pub(crate) fn len_and_flags(&self) -> (u32, u32) {
        let current = self.0.load(Ordering::Acquire);
        let len = current as u32;
        let flags = (current >> 32) as u32;
        (len, flags)
    }
}
