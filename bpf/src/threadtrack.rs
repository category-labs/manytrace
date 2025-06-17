mod threadtrack_skel {
    include!(concat!(env!("OUT_DIR"), "/threadtrack.skel.rs"));
}

use threadtrack_skel::*;

use crate::BpfError;
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{set_print, OpenObject, PrintLevel, RingBufferBuilder};
use protocol::{Event, ProcessName, ThreadName};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::convert::TryFrom;
use std::fs;
use std::mem::MaybeUninit;
use std::path::Path;
use std::rc::Rc;
use std::str;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadTrackerConfig {}

#[repr(C)]
#[derive(Debug)]
pub struct ThreadEvent {
    pub pid: u32,
    pub tgid: u32,
    pub comm: [u8; 16],
    pub filename: [u8; 256],
}

unsafe impl plain::Plain for ThreadEvent {}

impl ThreadEvent {
    pub fn comm_str(&self) -> &str {
        let len = self
            .comm
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.comm.len());
        str::from_utf8(&self.comm[..len]).expect("valid utf8")
    }

    pub fn filename_str(&self) -> &str {
        let len = self
            .filename
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.filename.len());
        str::from_utf8(&self.filename[..len]).expect("valid utf8")
    }
}

impl<'a> TryFrom<&'a [u8]> for &'a ThreadEvent {
    type Error = BpfError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        plain::from_bytes(data)
            .map_err(|e| BpfError::MapError(format!("failed to parse thread event: {:?}", e)))
    }
}

pub struct Object {
    object: MaybeUninit<libbpf_rs::OpenObject>,
}

impl Default for Object {
    fn default() -> Self {
        Self::new()
    }
}

impl Object {
    pub fn new() -> Self {
        Self {
            object: MaybeUninit::uninit(),
        }
    }

    pub fn build<'bd, F>(&'bd mut self, callback: F) -> Result<ThreadTracker<'bd, F>, BpfError>
    where
        F: for<'a> FnMut(Event<'a>) + 'bd,
    {
        ThreadTracker::new_with_debug(&mut self.object, callback, false)
    }
}

pub struct ThreadTracker<'this, F>
{
    _skel: ThreadtrackSkel<'this>,
    ringbuf: libbpf_rs::RingBuffer<'this>,
    callback: Rc<RefCell<F>>,
    proc_scanned: bool,
}

impl<'this, F> ThreadTracker<'this, F>
where
    F: for<'a> FnMut(Event<'a>) + 'this,
{
    fn new_with_debug(
        open_object: &'this mut MaybeUninit<OpenObject>,
        callback: F,
        debug: bool,
    ) -> Result<Self, BpfError> {
        if !debug {
            set_print(Some((PrintLevel::Debug, |_level, _msg| {})));
        }

        let skel_builder = ThreadtrackSkelBuilder::default();

        let open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {}", e)))?;

        skel.attach()
            .map_err(|e| BpfError::AttachError(format!("failed to attach bpf programs: {}", e)))?;

        let callback_rc = Rc::new(RefCell::new(callback));
        let callback_clone = callback_rc.clone();

        let mut builder = RingBufferBuilder::new();
        builder
            .add(&skel.maps.events, move |data| {
                let thread_event: &ThreadEvent = data.try_into().unwrap();
                let event = if !thread_event.filename_str().is_empty() {
                    Event::ProcessName(ProcessName {
                        name: thread_event.filename_str(),
                        pid: thread_event.tgid as i32,
                    })
                } else {
                    Event::ThreadName(ThreadName {
                        name: thread_event.comm_str(),
                        tid: thread_event.pid as i32,
                        pid: thread_event.tgid as i32,
                    })
                };

                callback_clone.borrow_mut()(event);
                0
            })
            .map_err(|e| BpfError::MapError(format!("failed to add ring buffer: {}", e)))?;

        let ringbuf = builder
            .build()
            .map_err(|e| BpfError::MapError(format!("failed to build ring buffer: {}", e)))?;

        Ok(ThreadTracker {
            _skel: skel,
            ringbuf,
            callback: callback_rc,
            proc_scanned: false,
        })
    }

    fn scan_proc(&mut self) -> Result<(), BpfError> {
        let proc_dir = Path::new("/proc");

        let entries = fs::read_dir(proc_dir)
            .map_err(|e| BpfError::MapError(format!("failed to read /proc: {}", e)))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            let file_name_os = entry.file_name();
            let file_name = match file_name_os.to_str() {
                Some(name) => name,
                None => continue,
            };

            let pid: i32 = match file_name.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            let comm_path = path.join("comm");
            if let Ok(comm) = fs::read_to_string(&comm_path) {
                let process_name = comm.trim().to_string();

                self.callback.borrow_mut()(Event::ProcessName(ProcessName {
                    name: process_name.as_str(),
                    pid,
                }));
            }

            let task_dir = path.join("task");
            if let Ok(tasks) = fs::read_dir(&task_dir) {
                for task in tasks {
                    let task = match task {
                        Ok(t) => t,
                        Err(_) => continue,
                    };

                    let task_file_name = task.file_name();
                    let task_name = match task_file_name.to_str() {
                        Some(name) => name,
                        None => continue,
                    };

                    let tid: i32 = match task_name.parse() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };

                    if tid == pid {
                        continue;
                    }

                    let thread_comm_path = task.path().join("comm");
                    if let Ok(comm) = fs::read_to_string(&thread_comm_path) {
                        let thread_name = comm.trim().to_string();

                        self.callback.borrow_mut()(Event::ThreadName(ThreadName {
                            name: thread_name.as_str(),
                            tid,
                            pid,
                        }));
                    }
                }
            }
        }

        Ok(())
    }

    pub fn poll(&mut self, timeout: Duration) -> Result<(), BpfError> {
        if !self.proc_scanned {
            self.scan_proc()?;
            self.proc_scanned = true;
        }

        self.ringbuf
            .poll(timeout)
            .map_err(|e| BpfError::MapError(format!("failed to poll ring buffer: {}", e)))?;

        Ok(())
    }

    pub fn consume(&mut self) -> Result<(), BpfError> {
        if !self.proc_scanned {
            self.scan_proc()?;
            self.proc_scanned = true;
        }

        self.ringbuf
            .consume()
            .map_err(|e| BpfError::MapError(format!("failed to consume ring buffer: {}", e)))?;

        Ok(())
    }
}
