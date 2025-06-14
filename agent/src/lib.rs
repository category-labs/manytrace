use mpscbuf::MpscBufError;
use thiserror::Error;

pub mod agent;
pub mod agent_state;
pub mod client;
pub mod epoll_thread;
pub mod mpsc;

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

pub(crate) fn get_timestamp_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}
