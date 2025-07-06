mod schedtrace_skel {
    include!(concat!(env!("OUT_DIR"), "/schedtrace.skel.rs"));
}

use schedtrace_skel::*;

use crate::{BpfError, Filterable};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, OpenObject, RingBufferBuilder};
use protocol::Message;
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::time::Duration;
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedTraceConfig {
    #[serde(default)]
    pub pid_filters: Vec<i32>,
    #[serde(default)]
    pub filter_process: Vec<String>,
}

pub struct Object {
    object: MaybeUninit<libbpf_rs::OpenObject>,
    config: SchedTraceConfig,
}

impl Object {
    pub fn new(config: SchedTraceConfig) -> Self {
        Self {
            object: MaybeUninit::uninit(),
            config,
        }
    }

    pub fn build<'bd, F>(&'bd mut self, callback: F) -> Result<SchedTrace<'bd, F>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) + 'bd,
    {
        let schedtrace = SchedTrace::new(&mut self.object, self.config.clone(), callback)?;
        Ok(schedtrace)
    }
}

pub struct SchedTrace<'this, F> {
    skel: SchedtraceSkel<'this>,
    ringbuf: libbpf_rs::RingBuffer<'this>,
    _phantom: PhantomData<F>,
}

impl<'this, F> SchedTrace<'this, F>
where
    F: for<'a> FnMut(Message<'a>) + 'this,
{
    fn new(
        open_object: &'this mut MaybeUninit<OpenObject>,
        config: SchedTraceConfig,
        _callback: F,
    ) -> Result<Self, BpfError> {
        let skel_builder = SchedtraceSkelBuilder::default();

        let mut open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        let filter_enabled = !config.pid_filters.is_empty() || !config.filter_process.is_empty();
        open_skel
            .maps
            .rodata_data
            .as_mut()
            .unwrap()
            .cfg
            .filter_enabled
            .write(filter_enabled);

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {}", e)))?;

        for &pid in &config.pid_filters {
            let key = pid.to_ne_bytes();
            let value = 1u32.to_ne_bytes();
            skel.maps
                .tracked_tgids
                .update(&key, &value, libbpf_rs::MapFlags::ANY)
                .map_err(|e| BpfError::MapError(format!("failed to update filter map: {}", e)))?;
        }

        skel.attach()
            .map_err(|e| BpfError::AttachError(format!("failed to attach bpf programs: {}", e)))?;

        let mut builder = RingBufferBuilder::new();
        builder
            .add(&skel.maps.events, move |data| {
                debug!("schedtrace event received");
                let _ = data;
                0
            })
            .map_err(|e| BpfError::MapError(format!("failed to add ring buffer: {}", e)))?;

        let ringbuf = builder
            .build()
            .map_err(|e| BpfError::MapError(format!("failed to build ring buffer: {}", e)))?;

        Ok(SchedTrace {
            skel,
            ringbuf,
            _phantom: PhantomData,
        })
    }

    pub fn poll(&mut self, timeout: Duration) -> Result<(), BpfError> {
        self.ringbuf
            .poll(timeout)
            .map_err(|e| BpfError::MapError(format!("failed to poll ring buffer: {}", e)))?;
        Ok(())
    }

    pub fn add_pid_filter(&mut self, pid: u32) -> Result<(), BpfError> {
        self.skel
            .maps
            .tracked_tgids
            .update(&pid.to_le_bytes(), &1u32.to_le_bytes(), MapFlags::ANY)
            .map_err(|e| BpfError::MapError(format!("failed to add PID filter: {}", e)))?;
        Ok(())
    }

    pub fn consume(&mut self) -> Result<(), BpfError> {
        self.ringbuf
            .consume()
            .map_err(|e| BpfError::MapError(format!("failed to consume ring buffer: {}", e)))?;
        Ok(())
    }
}

impl<'this, F> Filterable for SchedTrace<'this, F>
where
    F: for<'a> FnMut(Message<'a>) + 'this,
{
    fn filter(&mut self, pid: i32) -> Result<(), BpfError> {
        self.add_pid_filter(pid as u32)
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use protocol::{Event, TrackType};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Instant;

    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[derive(Debug)]
    enum TestMessage {
        Track { parent_is_thread: bool },
        Span,
        Other,
    }

    #[test]
    #[ignore = "requires root"]
    fn test_schedtrace_events() {
        assert!(is_root());

        let current_pid = std::process::id();
        let messages = Rc::new(RefCell::new(Vec::new()));
        let messages_clone = messages.clone();

        let config = SchedTraceConfig {
            pid_filters: vec![current_pid as i32],
            filter_process: vec![],
        };

        let mut object = Object::new(config);
        let mut schedtrace = object
            .build(move |message| {
                let test_msg = match message {
                    Message::Event(Event::Track(track)) => TestMessage::Track {
                        parent_is_thread: matches!(&track.parent, Some(TrackType::Thread { .. })),
                    },
                    Message::Event(Event::Span(_)) => TestMessage::Span,
                    _ => TestMessage::Other,
                };
                messages_clone.borrow_mut().push(test_msg);
            })
            .expect("failed to build schedtrace");

        let thread_handle = std::thread::spawn(move || {
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(100) {
            let _ = schedtrace.poll(Duration::from_millis(10));
        }

        thread_handle.join().expect("thread should complete");
        let _ = schedtrace.consume();

        let collected_messages = messages.borrow();

        let has_thread_track = collected_messages.iter().any(|msg| {
            matches!(
                msg,
                TestMessage::Track {
                    parent_is_thread: true
                }
            )
        });

        assert!(
            has_thread_track,
            "should have track descriptor with thread as parent"
        );

        let span_count = collected_messages
            .iter()
            .filter(|msg| matches!(msg, TestMessage::Span))
            .count();

        assert!(span_count > 0, "should have produced some spans");
    }
}
