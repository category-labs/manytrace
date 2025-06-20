use crate::{perf_event, BpfError, Filterable};
use blazesym::{
    symbolize::{
        source::{Process, Source},
        Input, Symbolizer,
    },
    Pid,
};
use libbpf_rs::{
    libbpf_sys,
    skel::{OpenSkel, SkelBuilder},
    MapCore, RingBuffer, RingBufferBuilder,
};
use protocol::{Callstack, CpuMode, Event, Frame, InternedData, InternedString, Sample};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem::MaybeUninit;
use std::time::Duration;
use std::{borrow::Cow, marker::PhantomData};
use std::{
    convert::TryFrom,
    os::fd::{AsFd, AsRawFd, RawFd},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfilerConfig {
    #[serde(default = "default_sample_freq")]
    pub sample_freq: u64,
    #[serde(default)]
    pub kernel_samples: bool,
    #[serde(default = "default_user_samples")]
    pub user_samples: bool,
    #[serde(default)]
    pub pid_filters: Vec<i32>,
    #[serde(default)]
    pub filter_process: Vec<String>,
}

fn default_sample_freq() -> u64 {
    99
}

fn default_user_samples() -> bool {
    true
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
    ) -> Result<Profiler<'obj, F>, BpfError>
    where
        F: for<'a> FnMut(Event<'a>) + 'obj,
    {
        Profiler::new(&mut self.object, callback, self.config.clone(), symbolizer)
    }
}

struct Callstacks {
    mapping: HashMap<(u32, u64), u64>,
    sequence: u64,
}

impl Callstacks {
    fn new() -> Self {
        Callstacks {
            mapping: HashMap::new(),
            sequence: 0,
        }
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
    F: for<'a> FnMut(Event<'a>) + 'obj,
{
    fn new(
        open_object: &'obj mut MaybeUninit<libbpf_rs::OpenObject>,
        mut callback: F,
        config: ProfilerConfig,
        symbolizer: &'obj Symbolizer,
    ) -> Result<Self, BpfError> {
        let skel_builder = ProfilerSkelBuilder::default();
        let mut open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        // Set filter_tgid based on whether we have any filters
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

        let pefds = perf_event::perf_event_per_cpu(perf_type, perf_config, config.sample_freq)
            .map_err(|e| BpfError::AttachError(format!("failed to create perf events: {}", e)))?;

        let links = perf_event::attach_perf_event(&pefds, &mut skel.progs.profiler_perf_event)
            .map_err(|e| BpfError::AttachError(format!("failed to attach perf event: {}", e)))?;
        let mut callstacks = Callstacks::new();

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
                let mut hasher = DefaultHasher::new();
                ustack_frames.hash(&mut hasher);
                kstack_frames.hash(&mut hasher);
                let rst = hasher.finish();
                let key = (event.tgid, rst);
                let callstack_iid = if let Some(&iid) = callstacks.mapping.get(&key) {
                    Some(iid)
                } else {
                    let usyms = symbolize_user_stack(symbolizer, &ustack_frames, event.tgid);
                    let ksyms = symbolize_kernel_stack(symbolizer, &kstack_frames);
                    if usyms.is_empty() && ksyms.is_empty() {
                        None
                    } else {
                        let iid = callstacks.sequence;

                        let mut intern_state = InternState::new();
                        intern_state.process_symbols(&usyms, &ustack_frames);
                        intern_state.process_symbols(&ksyms, &kstack_frames);

                        if intern_state.function_names.is_empty() && intern_state.frames.is_empty()
                        {
                            None
                        } else {
                            let interned_data = intern_state.build_interned_data(iid);
                            callback(Event::InternedData(interned_data));
                            callstacks.sequence += 1;
                            callstacks.mapping.insert(key, iid);
                            Some(iid)
                        }
                    }
                };
                if let Some(callstack_iid) = callstack_iid {
                    let sample = create_sample(event, callstack_iid);
                    callback(Event::Sample(sample));
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
    F: for<'a> FnMut(Event<'a>) + 'obj,
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

fn symbolize_user_stack<'a>(
    symbolizer: &'a Symbolizer,
    stack_frames: &[u64],
    pid: u32,
) -> Vec<blazesym::symbolize::Symbolized<'a>> {
    if stack_frames.is_empty() {
        return vec![];
    }

    symbolizer
        .symbolize(
            &Source::Process(Process {
                pid: Pid::from(pid),
                debug_syms: true,
                perf_map: false,
                map_files: true,
                _non_exhaustive: (),
            }),
            Input::AbsAddr(stack_frames),
        )
        .unwrap_or_else(|_| vec![])
}

fn symbolize_kernel_stack<'a>(
    symbolizer: &'a Symbolizer,
    stack_frames: &[u64],
) -> Vec<blazesym::symbolize::Symbolized<'a>> {
    if stack_frames.is_empty() {
        return vec![];
    }

    symbolizer
        .symbolize(
            &blazesym::symbolize::source::Source::Kernel(
                blazesym::symbolize::source::Kernel::default(),
            ),
            Input::AbsAddr(stack_frames),
        )
        .unwrap_or_else(|_| vec![])
}

fn create_sample(event: &PerfEvent, callstack_iid: u64) -> Sample {
    Sample {
        cpu: event.cpu_id,
        pid: event.tgid as i32,
        tid: event.pid as i32,
        timestamp: event.timestamp,
        callstack_iid,
        cpu_mode: CpuMode::Unknown,
    }
}

struct InternState<'a> {
    string_id_counter: u64,
    frame_id_counter: u64,
    function_names: Vec<InternedString<'a>>,
    frames: Vec<Frame>,
    frame_ids: Vec<u64>,
}

impl<'a> InternState<'a> {
    fn new() -> Self {
        let state: InternState<'a> = Self {
            string_id_counter: 0,
            frame_id_counter: 0,
            function_names: Vec::new(),
            frames: Vec::new(),
            frame_ids: Vec::new(),
        };
        state
    }

    fn add_symbol(&mut self, sym: &'a blazesym::symbolize::Sym, addr: u64) {
        let name = sym.name.as_ref();
        self.function_names.push(InternedString {
            iid: self.string_id_counter,
            str: Cow::Borrowed(name),
        });

        self.frames.push(Frame {
            iid: self.frame_id_counter,
            function_name_id: self.string_id_counter,
            mapping_id: 1,
            rel_pc: addr,
        });

        self.frame_ids.push(self.frame_id_counter);
        self.string_id_counter += 1;
        self.frame_id_counter += 1;
    }

    fn process_symbols(&mut self, syms: &'a [blazesym::symbolize::Symbolized], addrs: &[u64]) {
        for (sym, &addr) in syms.iter().zip(addrs.iter()).rev() {
            if let Some(sym) = sym.as_sym() {
                self.add_symbol(sym, addr);
            }
        }
    }

    fn build_interned_data(self, callstack_iid: u64) -> InternedData<'a> {
        let mappings = vec![protocol::Mapping {
            iid: 1,
            build_id: 1,
            exact_offset: 0,
            start_offset: 0,
            start: 0,
            end: 0x7fffffffffffffff,
            load_bias: 0,
            path_string_ids: vec![],
        }];

        let build_ids = vec![InternedString {
            iid: 1,
            str: Cow::Borrowed("unknown"),
        }];

        InternedData {
            function_names: self.function_names,
            frames: self.frames,
            callstacks: vec![Callstack {
                iid: callstack_iid,
                frame_ids: self.frame_ids,
            }],
            mappings,
            build_ids,
        }
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
            sample_freq: 99,
            kernel_samples: true,
            user_samples: true,
            pid_filters: vec![std::process::id() as i32],
            filter_process: vec![],
        };

        let mut object = Object::new(config);
        let mut profiler = object
            .build(
                move |event| match &event {
                    Event::Sample(sample) => {
                        *sample_count_ref += 1;
                        thread_ids_ref.insert(sample.tid);
                    }
                    Event::InternedData(data) => {
                        *interned_data_count_ref += 1;
                        for func in &data.function_names {
                            let name = func.str.to_string();
                            function_names_ref.push(name.clone());
                        }
                    }
                    _ => {}
                },
                &symbolizer,
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
