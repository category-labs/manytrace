//! # mpscbuf - Multi-Producer Single-Consumer Ring Buffer
//!
//! Ring buffer implementation supporting multiple producers and a single consumer,
//! based on the Linux kernel's BPF ring buffer design.
//!
//! Uses `memfd` and `mmap` for shared memory, atomic operations for synchronization,
//! and eventfd for notifications. Supports cross-process communication via file descriptors.
//!
//! ## Creating a Consumer
//!
//! Create a consumer with [`Consumer::new`]:
//!
//! ```rust
//! use mpscbuf::Consumer;
//!
//! let consumer = Consumer::new(1024 * 1024)?; // 1MB buffer
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! Buffer size must be a power of 2 and at least one page size (typically 4096 bytes).
//!
//! ## Creating a Producer
//!
//! Create a producer from file descriptors obtained from the consumer:
//!
//! ```rust
//! use mpscbuf::{Consumer, Producer, WakeupStrategy};
//!
//! # let consumer = Consumer::new(1024 * 1024)?;
//! let memory_fd = consumer.memory_fd().try_clone_to_owned()?;
//! let notification_fd = consumer.notification_fd().try_clone_to_owned()?;
//! let memory_size = consumer.memory_size();
//!
//! let producer = Producer::new(
//!     memory_fd,
//!     notification_fd,
//!     memory_size - 4096, // data size = total size - page size
//!     WakeupStrategy::Forced
//! )?;
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ## Producer Operations
//!
//! ### Writing Records
//!
//! ```rust
//! # use mpscbuf::{Consumer, Producer, WakeupStrategy};
//! # let consumer = Consumer::new(1024 * 1024)?;
//! # let memory_fd = consumer.memory_fd().try_clone_to_owned()?;
//! # let notification_fd = consumer.notification_fd().try_clone_to_owned()?;
//! # let producer = Producer::new(memory_fd, notification_fd, 1024 * 1024, WakeupStrategy::Forced)?;
//! let data = b"Hello, world!";
//!
//! if let Ok(mut reserved) = producer.reserve(data.len()) {
//!     reserved.copy_from_slice(data);
//!     // Automatically committed when dropped
//! }
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ### Discarding Records
//!
//! ```rust
//! # use mpscbuf::{Consumer, Producer, WakeupStrategy};
//! # let consumer = Consumer::new(1024 * 1024)?;
//! # let memory_fd = consumer.memory_fd().try_clone_to_owned()?;
//! # let notification_fd = consumer.notification_fd().try_clone_to_owned()?;
//! # let producer = Producer::new(memory_fd, notification_fd, 1024 * 1024, WakeupStrategy::Forced)?;
//! if let Ok(mut reserved) = producer.reserve(100) {
//!     reserved.discard(); // Don't commit
//! }
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ### Wakeup Strategies
//!
//! - **`WakeupStrategy::Forced`**: Always notify consumer after committing.
//! - **`WakeupStrategy::NoWakeup`**: Never notify consumer. Consumer must poll.
//!
//! ```rust
//! # use mpscbuf::{Consumer, Producer, WakeupStrategy};
//! # let consumer = Consumer::new(1024 * 1024)?;
//! # let memory_fd = consumer.memory_fd().try_clone_to_owned()?;
//! # let notification_fd = consumer.notification_fd().try_clone_to_owned()?;
//! let low_latency_producer = Producer::new(
//!     memory_fd.try_clone()?,
//!     notification_fd.try_clone()?,
//!     1024 * 1024,
//!     WakeupStrategy::Forced
//! )?;
//!
//! let no_wakeup_producer = Producer::new(
//!     memory_fd,
//!     notification_fd,
//!     1024 * 1024,
//!     WakeupStrategy::NoWakeup
//! )?;
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ### Tracking Dropped Records
//!
//! ```rust
//! # use mpscbuf::{Consumer, Producer, WakeupStrategy};
//! # let consumer = Consumer::new(1024 * 1024)?;
//! # let memory_fd = consumer.memory_fd().try_clone_to_owned()?;
//! # let notification_fd = consumer.notification_fd().try_clone_to_owned()?;
//! # let producer = Producer::new(memory_fd, notification_fd, 1024 * 1024, WakeupStrategy::Forced)?;
//! let data = b"Some data";
//!
//! if producer.reserve(data.len()).is_err() {
//!     producer.increment_dropped();
//! }
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ## Consumer Operations
//!
//! ### Reading Records
//!
//! ```rust
//! # use mpscbuf::Consumer;
//! # let mut consumer = Consumer::new(1024 * 1024)?;
//! while let Some(record) = consumer.next() {
//!     let data = record.as_slice();
//!     // Process data...
//!     // Automatically consumed when dropped
//! }
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ### Waiting for Data
//!
//! ```rust
//! # use mpscbuf::Consumer;
//! # let mut consumer = Consumer::new(1024 * 1024)?;
//! loop {
//!     let mut processed = 0;
//!     while let Some(record) = consumer.next() {
//!         // Process record...
//!         processed += 1;
//!     }
//!
//!     if processed == 0 {
//!         consumer.wait()?; // Block until notification
//!     }
//! }
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ### Monitoring
//!
//! ```rust
//! # use mpscbuf::Consumer;
//! # let mut consumer = Consumer::new(1024 * 1024)?;
//! let available_bytes = consumer.available_records();
//! let dropped = consumer.dropped();
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```
//!
//! ## Cross-Process Usage
//!
//! Consumer process creates the consumer and shares file descriptors.
//! Producer processes receive file descriptors and create producers.
//!
//! ```rust
//! # use mpscbuf::{Consumer, Producer, WakeupStrategy};
//! // Consumer process
//! let consumer = Consumer::new(8 * 1024 * 1024)?;
//!
//! let memory_fd = consumer.memory_fd();
//! let notification_fd = consumer.notification_fd();
//! let memory_size = consumer.memory_size();
//!
//! // Producer process (after receiving FDs)
//! # let memory_fd = memory_fd.try_clone_to_owned()?;
//! # let notification_fd = notification_fd.try_clone_to_owned()?;
//! let producer = Producer::new(
//!     memory_fd,
//!     notification_fd,
//!     memory_size - 4096,
//!     WakeupStrategy::Forced
//! )?;
//! # Ok::<(), mpscbuf::MpscBufError>(())
//! ```

pub use consumer::{Consumer, Record};
pub use error::MpscBufError;
pub use producer::{Producer, ReservedBuffer, WakeupStrategy};

pub(crate) mod common;
pub mod consumer;
pub mod error;
#[cfg(all(test, feature = "loom"))]
pub(crate) mod loom;
pub(crate) mod memory;
pub(crate) mod producer;
pub(crate) mod ringbuf;
pub(crate) mod sync;
