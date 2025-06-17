//! High-performance event collection agent using shared memory IPC.
//!
//! # Example
//!
//! ```no_run
//! use agent::{Agent, AgentClient, Consumer};
//! use protocol::{Event, Counter, Labels, LogLevel};
//!
//! // Server side - receives events
//! let agent = Agent::new("/tmp/agent.sock".to_string())?;
//!
//! // Client side - sends events  
//! let mut consumer = Consumer::new(1024 * 1024)?;
//! let mut client = AgentClient::new("/tmp/agent.sock".to_string());
//! client.start(&consumer, LogLevel::Debug)?;
//!
//! // Submit events through the agent
//! let event = Event::Counter(Counter {
//!     name: "requests",
//!     value: 1.0,
//!     timestamp: 123456789,
//!     tid: 1,
//!     pid: 100,
//!     labels: std::borrow::Cow::Owned(Labels::new()),
//! });
//! agent.submit(&event)?;
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
use thiserror::Error;

pub use agent::Agent;
pub use client::AgentClient;
pub use mpsc::{Consumer, Producer};

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
}

pub type Result<T> = std::result::Result<T, AgentError>;

pub(crate) mod agent;
pub(crate) mod agent_state;
pub(crate) mod client;
pub(crate) mod epoll_thread;
pub(crate) mod mpsc;
