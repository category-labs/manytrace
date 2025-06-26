//! High-performance event collection agent using shared memory IPC.
//!
//! # Example
//!
//! ```no_run
//! use agent::{Agent, AgentClient, Consumer};
//!
//! // Server side - receives events
//! let agent = Agent::new("/tmp/agent.sock".to_string())?;
//!
//! // Client side - sends events  
//! let mut consumer = Consumer::new(1024 * 1024)?;
//! let mut client = AgentClient::new("/tmp/agent.sock".to_string());
//! client.start(&consumer, "debug".to_owned())?;
//!
//!
//! // Send periodic keepalive to prevent timeout
//! client.send_continue()?;
//!
//! // Read events on consumer side
//! while let Some(record) = consumer.consume() {
//!     if let Ok(event) = record.as_event() {
//!         // Process event
//!     }
//! }
//!
//! // Cleanup
//! client.stop()?;
//! # Ok::<(), agent::AgentError>(())
//! ```

use mpscbuf::MpscBufError;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use thiserror::Error;

pub use agent::{Agent, AgentBuilder};
pub use client::AgentClient;
pub use extension::{AgentHandle, Extension, ExtensionError};
pub use mpsc::{Consumer, Producer};

pub const RANDOM_PROCESS_ID_OPTION: &str = "random_process_id";

static PROCESS_ID: OnceLock<AtomicI32> = OnceLock::new();

fn get_process_id_atomic() -> &'static AtomicI32 {
    PROCESS_ID.get_or_init(|| AtomicI32::new(std::process::id() as i32))
}

pub fn get_process_id() -> i32 {
    get_process_id_atomic().load(Ordering::Relaxed)
}

pub(crate) fn set_process_id(id: i32) {
    get_process_id_atomic().store(id, Ordering::Relaxed);
}

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("System error: {0}")]
    Nix(#[from] nix::Error),
    #[error("MPSC buffer error: {0}")]
    Mpscbuf(#[from] MpscBufError),
    #[error("Archive error: {0}")]
    Archive(#[from] rkyv::rancor::Error),
    #[error("Agent not enabled")]
    NotEnabled,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

pub(crate) mod agent;
pub(crate) mod agent_state;
pub(crate) mod client;
pub(crate) mod epoll_thread;
pub(crate) mod extension;
pub(crate) mod mpsc;
