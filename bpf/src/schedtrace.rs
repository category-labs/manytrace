mod schedtrace_skel {
    include!(concat!(env!("OUT_DIR"), "/schedtrace.skel.rs"));
}

use blazesym::symbolize::source::{Kernel, Source};
use schedtrace_skel::*;

use crate::{BpfError, Filterable};
use blazesym::symbolize::{Input, Sym, Symbolized, Symbolizer};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, OpenObject, RingBufferBuilder};
use protocol::{Event, Labels, Message, Span, Track, TrackId, TrackType};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::time::Duration;
use tracing::debug;

const SCHED_EVENT_BLOCKED: u32 = 1;
const SCHED_EVENT_WAKING: u32 = 2;

fn default_ringbuf_size() -> usize {
    2 * 1024 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedTraceConfig {
    #[serde(default)]
    pub pid_filters: Vec<i32>,
    #[serde(default)]
    pub filter_process: Vec<String>,
    #[serde(default = "default_ringbuf_size")]
    pub ringbuf: usize,
}

#[repr(C)]
#[derive(Debug)]
pub struct SchedSpanEvent {
    pub pid: u32,
    pub tid: u32,
    pub start_time: u64,
    pub end_time: u64,
    pub cpu: u32,
    pub frame: u64,
    pub event_type: u32,
}

unsafe impl plain::Plain for SchedSpanEvent {}

impl<'a> TryFrom<&'a [u8]> for &'a SchedSpanEvent {
    type Error = BpfError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        plain::from_bytes(data)
            .map_err(|e| BpfError::MapError(format!("failed to parse sched span event: {:?}", e)))
    }
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

    pub fn build<'bd, F>(
        &'bd mut self,
        callback: F,
        symbolizer: &'bd Symbolizer,
    ) -> Result<SchedTrace<'bd, F>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) -> i32 + 'bd,
    {
        let schedtrace =
            SchedTrace::new(&mut self.object, self.config.clone(), callback, symbolizer)?;
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
    F: for<'a> FnMut(Message<'a>) -> i32 + 'this,
{
    fn new(
        open_object: &'this mut MaybeUninit<OpenObject>,
        config: SchedTraceConfig,
        mut callback: F,
        symbolizer: &'this Symbolizer,
    ) -> Result<Self, BpfError> {
        let skel_builder = SchedtraceSkelBuilder::default();

        let mut open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        open_skel
            .maps
            .events
            .set_max_entries(config.ringbuf as u32)
            .map_err(|e| BpfError::LoadError(format!("failed to set ring buffer size: {}", e)))?;

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

        let mut thread_track_ids: HashMap<(i32, i32), u64> = HashMap::new();
        let mut rng = rand::thread_rng();

        let symbolizer_ref = symbolizer;
        let mut builder = RingBufferBuilder::new();
        builder
            .add(&skel.maps.events, move |data| {
                match <&SchedSpanEvent>::try_from(data) {
                    Ok(event) => {
                        debug!(
                            pid = event.pid,
                            tid = event.tid,
                            cpu = event.cpu,
                            duration_ns = event.end_time - event.start_time,
                            "schedtrace span"
                        );

                        let key = (event.pid as i32, event.tid as i32);
                        let track_id_value = thread_track_ids.entry(key).or_insert_with(|| {
                            let id = rng.gen::<u64>();
                            let track = Track {
                                name: "kernel",
                                track_type: TrackType::Custom { id },
                                parent: Some(TrackType::Thread {
                                    pid: event.pid as i32,
                                    tid: event.tid as i32,
                                }),
                            };
                            callback(Message::Event(Event::Track(track)));
                            id
                        });

                        let track_id = TrackId::Custom {
                            id: *track_id_value,
                        };

                        let span_name = match event.event_type {
                            SCHED_EVENT_BLOCKED if event.frame != 0 => {
                                match resolve_kernel_symbol(symbolizer_ref, event.frame) {
                                    Some(sym) => format!("waiting [{}]", sym.name),
                                    None => format!("waiting [0x{:x}]", event.frame),
                                }
                            }
                            SCHED_EVENT_BLOCKED => "waiting".to_string(),
                            SCHED_EVENT_WAKING => "waking".to_string(),
                            _ => "running".to_string(),
                        };

                        let mut labels = Labels::new();
                        labels.ints.insert("cpu", event.cpu as i64);

                        let span = Span {
                            name: span_name.as_str(),
                            span_id: event.start_time,
                            start_timestamp: event.start_time,
                            end_timestamp: event.end_time,
                            track_id,
                            labels: Cow::Owned(labels),
                        };
                        let result = callback(Message::Event(Event::Span(span)));
                        if result != 0 {
                            return result;
                        }
                    }
                    Err(e) => {
                        debug!(error = %e, "failed to parse sched span event");
                    }
                }

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
    F: for<'a> FnMut(Message<'a>) -> i32 + 'this,
{
    fn filter(&mut self, pid: i32) -> Result<(), BpfError> {
        self.add_pid_filter(pid as u32)
    }
}

fn resolve_kernel_symbol<'a>(symbolizer: &'a Symbolizer, addr: u64) -> Option<Sym<'a>> {
    let source = Source::Kernel(Kernel::default());
    let addresses = vec![addr];

    match symbolizer.symbolize(&source, Input::AbsAddr(&addresses)) {
        Ok(results) => {
            for result in results {
                match result {
                    Symbolized::Sym(sym) => return Some(sym),
                    _ => continue,
                }
            }
            None
        }
        Err(e) => {
            debug!(error = %e, addr = %addr, "failed to symbolize kernel address");
            None
        }
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
        Span { name: String, has_cpu_label: bool },
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
            ringbuf: default_ringbuf_size(),
        };

        let symbolizer = Symbolizer::new();
        let mut object = Object::new(config);
        let mut schedtrace = object
            .build(
                move |message| {
                    let test_msg = match message {
                        Message::Event(Event::Track(track)) => TestMessage::Track {
                            parent_is_thread: matches!(
                                &track.parent,
                                Some(TrackType::Thread { .. })
                            ),
                        },
                        Message::Event(Event::Span(span)) => TestMessage::Span {
                            name: span.name.to_string(),
                            has_cpu_label: span.labels.ints.contains_key("cpu"),
                        },
                        _ => TestMessage::Other,
                    };
                    messages_clone.borrow_mut().push(test_msg);
                    0
                },
                &symbolizer,
            )
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

        let span_messages: Vec<_> = collected_messages
            .iter()
            .filter_map(|msg| match msg {
                TestMessage::Span {
                    name,
                    has_cpu_label,
                } => Some((name, has_cpu_label)),
                _ => None,
            })
            .collect();

        assert!(!span_messages.is_empty(), "should have produced some spans");

        for (span_name, has_cpu) in &span_messages {
            assert!(*has_cpu, "span event '{}' should have cpu label", span_name);
        }
    }
}
