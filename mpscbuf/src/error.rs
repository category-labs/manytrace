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

use thiserror::Error;

#[derive(Error, Debug)]
pub enum MpscBufError {
    #[error("size must be at least twice the page size ({0} bytes)")]
    SizeTooSmall(usize),

    #[error("size must be a multiple of page size ({0} bytes)")]
    SizeNotAligned(usize),

    #[error("memory mapping failed: {0}")]
    MmapFailed(#[from] nix::errno::Errno),

    #[error("memory protection failed: {0}")]
    MprotectFailed(nix::errno::Errno),

    #[error("insufficient space in ring buffer. producer position: {0}, consumer position: {1}, data size: {2}")]
    InsufficientSpace(u64, u64, u64),

    #[error("invalid record size: {0}")]
    InvalidRecordSize(usize),

    #[error("eventfd creation failed: {0}")]
    EventfdCreation(String),

    #[error("eventfd write failed: {0}")]
    EventfdWrite(String),

    #[error("eventfd read failed: {0}")]
    EventfdRead(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("failed to acquire spinlock within timeout")]
    LockTimeout,
}
