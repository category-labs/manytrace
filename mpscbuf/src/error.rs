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
    
    #[error("insufficient space in ring buffer")]
    InsufficientSpace,
    
    #[error("invalid record size: {0}")]
    InvalidRecordSize(usize),
}