pub mod error {
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum BpfError {
        #[error("failed to load bpf program: {0}")]
        LoadError(String),
        #[error("failed to attach bpf program: {0}")]
        AttachError(String),
        #[error("bpf map operation failed: {0}")]
        MapError(String),
    }
}

pub use error::BpfError;

pub trait Consumer {
    fn consume(&mut self) -> Result<(), BpfError>;
}

mod threadtrack;
pub use threadtrack::{ThreadTracker, ThreadTrackerBuilder};

mod cpuutil;
pub use cpuutil::{CpuUtil, CpuUtilBuilder};

mod perf_event;
