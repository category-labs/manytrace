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

#[path = "bpf/threadtrack.skel.rs"]
mod threadtrack_skel;

use threadtrack_skel::*;

use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{set_print, PrintLevel, RingBufferBuilder};
use std::mem::MaybeUninit;
use std::time::Duration;

#[repr(C)]
#[derive(Debug, Clone)]
pub struct ThreadEvent {
    pub pid: u32,
    pub tgid: u32,
    pub comm: [u8; 16],
    pub filename: [u8; 256],
}

unsafe impl plain::Plain for ThreadEvent {}

impl ThreadEvent {
    pub fn comm_str(&self) -> std::borrow::Cow<str> {
        let len = self
            .comm
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.comm.len());
        String::from_utf8_lossy(&self.comm[..len])
    }

    pub fn filename_str(&self) -> std::borrow::Cow<str> {
        let len = self
            .filename
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.filename.len());
        String::from_utf8_lossy(&self.filename[..len])
    }
}

pub struct ThreadTracker {
    _skel: ThreadtrackSkel<'static>,
}

impl ThreadTracker {
    pub fn new() -> Result<Self, BpfError> {
        Self::new_with_debug(false)
    }

    pub fn new_with_debug(debug: bool) -> Result<Self, BpfError> {
        if !debug {
            set_print(Some((PrintLevel::Debug, |_level, _msg| {})));
        }

        let skel_builder = ThreadtrackSkelBuilder::default();

        let open_object = Box::new(MaybeUninit::uninit());
        let open_object_ptr = Box::leak(open_object);

        let open_skel = skel_builder
            .open(open_object_ptr)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {}", e)))?;

        skel.attach()
            .map_err(|e| BpfError::AttachError(format!("failed to attach bpf programs: {}", e)))?;

        Ok(ThreadTracker { _skel: skel })
    }

    pub fn poll_events<F>(&self, timeout: Duration, mut callback: F) -> Result<(), BpfError>
    where
        F: FnMut(&ThreadEvent),
    {
        let mut builder = RingBufferBuilder::new();
        builder
            .add(&self._skel.maps.events, |data| {
                let event =
                    plain::from_bytes::<ThreadEvent>(data).expect("failed to parse thread event");
                callback(event);
                0
            })
            .map_err(|e| BpfError::MapError(format!("failed to add ring buffer: {}", e)))?;

        let ringbuf = builder
            .build()
            .map_err(|e| BpfError::MapError(format!("failed to build ring buffer: {}", e)))?;

        ringbuf
            .poll(timeout)
            .map_err(|e| BpfError::MapError(format!("failed to poll ring buffer: {}", e)))?;

        Ok(())
    }
}
