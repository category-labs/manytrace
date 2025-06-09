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
}
