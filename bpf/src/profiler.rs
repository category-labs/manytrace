use crate::{perf_event, BpfError, Filterable};
use blazesym::symbolize::Symbolizer;
use interned_symbolizer::{ProcessConfig, Symbolizer as InternedSymbolizer};
use libbpf_rs::{
    libbpf_sys,
    skel::{OpenSkel, SkelBuilder},
    MapCore, RingBuffer, RingBufferBuilder,
};
use protocol::{CpuMode, Event, Message, Sample, TrackId};
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::time::Duration;
use std::{
    convert::TryFrom,
    os::fd::{AsFd, AsRawFd, RawFd},
};
use tracing::warn;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfilerConfig {
    #[serde(default = "default_frequency")]
    pub frequency: u64,
    #[serde(default)]
    pub kernel_samples: bool,
    #[serde(default = "default_user_samples")]
    pub user_samples: bool,
    #[serde(default)]
    pub pid_filters: Vec<i32>,
    #[serde(default)]
    pub filter_process: Vec<String>,
    #[serde(default = "default_debug_syms")]
    pub debug_syms: bool,
    #[serde(default)]
    pub perf_map: bool,
    #[serde(default = "default_map_files")]
    pub map_files: bool,
    #[serde(default = "default_ringbuf_size")]
    pub ringbuf: usize,
}

fn default_frequency() -> u64 {
    99
}

fn default_user_samples() -> bool {
    true
}

fn default_debug_syms() -> bool {
    true
}

fn default_map_files() -> bool {
    true
}

fn default_ringbuf_size() -> usize {
    256 * 1024
}

mod profiler_bpf {
    include!(concat!(env!("OUT_DIR"), "/profiler.skel.rs"));
}

use profiler_bpf::*;

#[repr(C)]
#[derive(Debug)]
pub struct PerfEvent {
    pub timestamp: u64,
    pub tgid: u32,
    pub pid: u32,
    pub cpu_id: u32,
    pub ustack_id: i32,
    pub kstack_id: i32,
}

unsafe impl plain::Plain for PerfEvent {}

impl<'a> TryFrom<&'a [u8]> for &'a PerfEvent {
    type Error = BpfError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        plain::from_bytes(data)
            .map_err(|e| BpfError::MapError(format!("failed to parse perf event: {:?}", e)))
    }
}

pub struct Object {
    object: MaybeUninit<libbpf_rs::OpenObject>,
    config: ProfilerConfig,
}

impl Object {
    pub fn new(config: ProfilerConfig) -> Self {
        Object {
            object: MaybeUninit::uninit(),
            config,
        }
    }

    pub fn build<'obj, F>(
        &'obj mut self,
        callback: F,
        symbolizer: &'obj Symbolizer,
        stream_id: crate::StreamId,
    ) -> Result<Profiler<'obj, F>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) + 'obj,
    {
        Profiler::new(
            &mut self.object,
            callback,
            self.config.clone(),
            symbolizer,
            stream_id,
        )
    }
}

pub struct Profiler<'obj, F> {
    _skel: ProfilerSkel<'obj>,
    rb: RingBuffer<'obj>,
    _links: Vec<libbpf_rs::Link>,
    _phantom: PhantomData<F>,
}

impl<'obj, F> Profiler<'obj, F>
where
    F: for<'a> FnMut(Message<'a>) + 'obj,
{
    fn new(
        open_object: &'obj mut MaybeUninit<libbpf_rs::OpenObject>,
        mut callback: F,
        config: ProfilerConfig,
        symbolizer: &'obj Symbolizer,
        stream_id: crate::StreamId,
    ) -> Result<Self, BpfError> {
        let skel_builder = ProfilerSkelBuilder::default();
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
            .filter_tgid
            .write(filter_enabled);
        open_skel
            .maps
            .rodata_data
            .as_mut()
            .unwrap()
            .cfg
            .collect_kstack
            .write(config.kernel_samples);
        open_skel
            .maps
            .rodata_data
            .as_mut()
            .unwrap()
            .cfg
            .collect_ustack
            .write(config.user_samples);

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {}", e)))?;

        for &pid in &config.pid_filters {
            let key = pid.to_ne_bytes();
            let value = 1u8.to_ne_bytes();
            skel.maps
                .filter_tgid
                .update(&key, &value, libbpf_rs::MapFlags::ANY)
                .map_err(|e| BpfError::MapError(format!("failed to update filter map: {}", e)))?;
        }

        let perf_type = libbpf_sys::PERF_TYPE_SOFTWARE;
        let perf_config = libbpf_sys::PERF_COUNT_SW_CPU_CLOCK;

        let pefds = perf_event::perf_event_per_cpu(perf_type, perf_config, config.frequency)
            .map_err(|e| BpfError::AttachError(format!("failed to create perf events: {}", e)))?;

        let links = perf_event::attach_perf_event(&pefds, &mut skel.progs.profiler_perf_event)
            .map_err(|e| BpfError::AttachError(format!("failed to attach perf event: {}", e)))?;

        let process_config = ProcessConfig {
            debug_syms: config.debug_syms,
            perf_map: config.perf_map,
            map_files: config.map_files,
        };
        let mut interned_symbolizer = InternedSymbolizer::new(process_config);

        // NOTE find a way to handle it in a safer manner
        let stackmap_fd = skel.maps.stackmap.as_fd().as_raw_fd();

        let mut builder = RingBufferBuilder::new();
        builder
            .add(&skel.maps.events, move |data: &[u8]| {
                let event: &PerfEvent = data.try_into().unwrap();

                let ustack_frames = if event.ustack_id >= 0 {
                    get_stack_frames_by_fd(stackmap_fd, event.ustack_id)
                } else {
                    vec![]
                };
                let kstack_frames = if event.kstack_id >= 0 {
                    get_stack_frames_by_fd(stackmap_fd, event.kstack_id)
                } else {
                    vec![]
                };
                if ustack_frames.is_empty() && kstack_frames.is_empty() {
                    return 0;
                }

                let pid = event.tgid as i32;

                let mut session = interned_symbolizer.session(symbolizer);
                match session.symbolize(pid, &ustack_frames, &kstack_frames) {
                    Ok(mut interned) => {
                        let (callstack_iid, interned_data_opt) = interned.data();

                        if let Some(interned_data) = interned_data_opt {
                            callback(Message::Stream {
                                stream_id,
                                event: Event::InternedData(interned_data),
                            });
                        }

                        let sample = create_sample(event, callstack_iid);
                        callback(Message::Stream {
                            stream_id,
                            event: Event::Sample(sample),
                        });
                    }
                    Err(err) => {
                        warn!(err = %err, pid = %pid, "failed to symbolize stack");
                    }
                }
                0
            })
            .map_err(|e| BpfError::LoadError(format!("failed to add ring buffer: {}", e)))?;

        let rb = builder
            .build()
            .map_err(|e| BpfError::LoadError(format!("failed to build ring buffer: {}", e)))?;

        Ok(Profiler {
            _skel: skel,
            rb,
            _links: links,
            _phantom: PhantomData,
        })
    }

    pub fn consume(&mut self) -> Result<(), BpfError> {
        match self.rb.consume() {
            Ok(_) => Ok(()),
            Err(e) => Err(BpfError::MapError(format!(
                "failed to consume events: {}",
                e
            ))),
        }
    }

    pub fn poll(&mut self, timeout: Duration) -> Result<(), BpfError> {
        match self.rb.poll(timeout) {
            Ok(_) => Ok(()),
            Err(e) => Err(BpfError::MapError(format!("failed to poll events: {}", e))),
        }
    }
}

impl<'obj, F> Filterable for Profiler<'obj, F>
where
    F: for<'a> FnMut(Message<'a>) + 'obj,
{
    fn filter(&mut self, pid: i32) -> Result<(), BpfError> {
        let key = pid.to_ne_bytes();
        let value = 1u8.to_ne_bytes();
        self._skel
            .maps
            .filter_tgid
            .update(&key, &value, libbpf_rs::MapFlags::ANY)
            .map_err(|e| BpfError::MapError(format!("failed to update filter map: {}", e)))?;
        Ok(())
    }
}

fn get_stack_frames_by_fd(stackmap_fd: RawFd, stack_id: i32) -> Vec<u64> {
    unsafe {
        let mut frames = vec![0u64; 127];
        let key = stack_id.to_ne_bytes();
        let ret = libbpf_sys::bpf_map_lookup_elem(
            stackmap_fd,
            key.as_ptr() as *const _,
            frames.as_mut_ptr() as *mut _,
        );

        if ret == 0 {
            let last_non_zero = frames.iter().rposition(|&x| x != 0).unwrap_or(0);
            frames.truncate(last_non_zero + 1);
            frames
        } else {
            vec![]
        }
    }
}

fn create_sample(event: &PerfEvent, callstack_iid: u64) -> Sample {
    Sample {
        cpu: event.cpu_id,
        track_id: TrackId::Thread {
            tid: event.pid as i32,
            pid: event.tgid as i32,
        },
        timestamp: event.timestamp,
        callstack_iid,
        cpu_mode: CpuMode::Unknown,
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::thread;
    use tempfile::tempdir;

    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    #[ignore = "requires root"]
    fn test_profiler_writes_samples_to_file() {
        assert!(is_root());

        let dir = tempdir().expect("Failed to create temp dir");
        let file_path = dir.path().join("test_samples.bin");
        let file_path_clone = file_path.clone();

        let mut sample_count = 0;
        let mut interned_data_count = 0;
        let mut thread_ids = std::collections::HashSet::new();
        let mut function_names = Vec::new();

        let sample_count_ref = &mut sample_count;
        let interned_data_count_ref = &mut interned_data_count;
        let thread_ids_ref = &mut thread_ids;
        let function_names_ref = &mut function_names;

        let symbolizer = Symbolizer::new();
        let config = ProfilerConfig {
            frequency: 99,
            kernel_samples: true,
            user_samples: true,
            pid_filters: vec![std::process::id() as i32],
            filter_process: vec![],
            debug_syms: true,
            perf_map: false,
            map_files: true,
            ringbuf: default_ringbuf_size(),
        };

        let mut object = Object::new(config);
        let mut profiler = object
            .build(
                move |message| {
                    let event = match &message {
                        Message::Stream { event, .. } => event,
                        Message::Event(e) => e,
                    };
                    match event {
                        Event::Sample(sample) => {
                            *sample_count_ref += 1;
                            if let protocol::TrackId::Thread { tid, .. } = sample.track_id {
                                thread_ids_ref.insert(tid);
                            }
                        }
                        Event::InternedData(data) => {
                            *interned_data_count_ref += 1;
                            for func in &data.function_names {
                                let name = func.str.to_string();
                                function_names_ref.push(name.clone());
                            }
                        }
                        _ => {}
                    }
                },
                &symbolizer,
                0,
            )
            .expect("failed to create profiler");

        let test_thread = thread::Builder::new()
            .name("test_thread".to_string())
            .spawn(move || {
                let mut file = File::create(&file_path_clone).expect("Failed to create temp file");
                let data = vec![1u8; 1024];
                let start = std::time::Instant::now();
                let mut write_count = 0;

                while start.elapsed() < std::time::Duration::from_millis(500) {
                    file.write_all(&data).expect("failed to write");
                    write_count += 1;

                    if write_count % 100 == 0 {
                        file.flush().expect("failed to flush");
                    }
                }

                unsafe { libc::gettid() as usize }
            })
            .expect("failed to spawn thread");

        let test_thread_id = test_thread.join().unwrap();

        profiler.consume().unwrap();
        drop(profiler);

        assert!(
            sample_count > 0,
            "should have collected at least one sample"
        );
        assert!(
            interned_data_count > 0,
            "should have collected at least one interned data event"
        );
        assert!(
            thread_ids.contains(&(test_thread_id as i32)),
            "samples should contain the test thread ID"
        );
        let has_write_function = function_names.iter().any(|name| name.contains("write"));
        assert!(
            has_write_function,
            "Should have found at least one write-related function in stack traces"
        );
    }
}
