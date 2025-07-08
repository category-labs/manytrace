use crate::{perf_event, BpfError, Filterable};
use libbpf_rs::skel::{OpenSkel, SkelBuilder};
use libbpf_rs::{MapCore, RingBuffer, RingBufferBuilder};
use libbpf_sys;
use protocol::{Event, Message};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::time::Duration;
use std::{convert::TryFrom, os::fd::RawFd};

pub const MAX_PERF_COUNTERS: usize = 8;

const PERF_EVENT_IOC_ENABLE: u64 = 0x2400;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CounterConfig {
    Named(String),
    Custom {
        name: String,
        #[serde(rename = "type")]
        perf_type: u32,
        config: u64,
    },
}

#[derive(Debug, Clone)]
pub enum DerivedCounter {
    Ipc {
        cycles_idx: usize,
        instructions_idx: usize,
    },
}

impl CounterConfig {
    pub fn to_perf_config(&self) -> Result<(String, u32, u64), BpfError> {
        match self {
            CounterConfig::Named(name) => match name.as_str() {
                "cpu-cycles" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_CPU_CYCLES as u64,
                )),
                "cpu-instructions" | "instructions" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_INSTRUCTIONS as u64,
                )),
                "cache-references" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_CACHE_REFERENCES as u64,
                )),
                "cache-misses" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_CACHE_MISSES as u64,
                )),
                "branch-instructions" | "branches" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_BRANCH_INSTRUCTIONS as u64,
                )),
                "branch-misses" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_BRANCH_MISSES as u64,
                )),
                "stalled-cycles-frontend" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_STALLED_CYCLES_FRONTEND as u64,
                )),
                "stalled-cycles-backend" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_HARDWARE,
                    libbpf_sys::PERF_COUNT_HW_STALLED_CYCLES_BACKEND as u64,
                )),

                "cpu-clock" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_SOFTWARE,
                    libbpf_sys::PERF_COUNT_SW_CPU_CLOCK as u64,
                )),
                "task-clock" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_SOFTWARE,
                    libbpf_sys::PERF_COUNT_SW_TASK_CLOCK as u64,
                )),
                "page-faults" | "faults" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_SOFTWARE,
                    libbpf_sys::PERF_COUNT_SW_PAGE_FAULTS as u64,
                )),
                "context-switches" | "cs" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_SOFTWARE,
                    libbpf_sys::PERF_COUNT_SW_CONTEXT_SWITCHES as u64,
                )),
                "cpu-migrations" | "migrations" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_SOFTWARE,
                    libbpf_sys::PERF_COUNT_SW_CPU_MIGRATIONS as u64,
                )),
                "minor-faults" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_SOFTWARE,
                    libbpf_sys::PERF_COUNT_SW_PAGE_FAULTS_MIN as u64,
                )),
                "major-faults" => Ok((
                    name.clone(),
                    libbpf_sys::PERF_TYPE_SOFTWARE,
                    libbpf_sys::PERF_COUNT_SW_PAGE_FAULTS_MAJ as u64,
                )),

                _ => Err(BpfError::LoadError(format!(
                    "unknown performance counter: {}",
                    name
                ))),
            },
            CounterConfig::Custom {
                name,
                perf_type,
                config,
            } => Ok((name.clone(), *perf_type, *config)),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfCounterConfig {
    #[serde(default = "default_frequency")]
    pub frequency: u64,
    pub counters: Vec<CounterConfig>,
    #[serde(default)]
    pub pid_filters: Vec<i32>,
    #[serde(default)]
    pub filter_process: Vec<String>,
    #[serde(default = "default_ringbuf_size")]
    pub ringbuf: usize,
}

impl PerfCounterConfig {
    pub fn expand_counters(&mut self) -> Result<Vec<(String, Option<DerivedCounter>)>, BpfError> {
        let mut expanded = Vec::new();
        let mut derived_counters = Vec::new();
        let mut current_idx = 0;

        for counter in &self.counters {
            match counter {
                CounterConfig::Named(name) if name == "ipc" => {
                    let cycles_idx = current_idx;
                    let instructions_idx = current_idx + 1;

                    expanded.push(CounterConfig::Named("cpu-cycles".to_string()));
                    expanded.push(CounterConfig::Named("cpu-instructions".to_string()));

                    derived_counters.push((
                        "ipc".to_string(),
                        Some(DerivedCounter::Ipc {
                            cycles_idx,
                            instructions_idx,
                        }),
                    ));
                    current_idx += 2;
                }
                _ => {
                    expanded.push(counter.clone());
                    derived_counters.push(("".to_string(), None));
                    current_idx += 1;
                }
            }
        }

        self.counters = expanded;
        Ok(derived_counters)
    }
}

fn default_frequency() -> u64 {
    99
}

fn default_ringbuf_size() -> usize {
    256 * 1024
}

impl PerfCounterConfig {
    pub fn validate(&mut self) -> Result<Vec<(String, Option<DerivedCounter>)>, BpfError> {
        let derived_info = self.expand_counters()?;

        if self.counters.is_empty() {
            return Err(BpfError::LoadError(
                "no performance counters configured".to_string(),
            ));
        }
        if self.counters.len() > MAX_PERF_COUNTERS {
            return Err(BpfError::LoadError(format!(
                "too many performance counters: {} (max: {})",
                self.counters.len(),
                MAX_PERF_COUNTERS
            )));
        }

        for counter in &self.counters {
            counter.to_perf_config()?;
        }

        Ok(derived_info)
    }
}

mod perfcounter_bpf {
    include!(concat!(env!("OUT_DIR"), "/perfcounter.skel.rs"));
}
use perfcounter_bpf::*;

#[repr(C)]
#[derive(Debug)]
pub struct PerfCounterEvent {
    pub timestamp: u64,
    pub cpu_id: u32,
    pub padding: u32,
    pub counters: [u64; MAX_PERF_COUNTERS],
}

unsafe impl plain::Plain for PerfCounterEvent {}

impl<'a> TryFrom<&'a [u8]> for &'a PerfCounterEvent {
    type Error = BpfError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        plain::from_bytes(data)
            .map_err(|e| BpfError::MapError(format!("failed to parse perf counter event: {:?}", e)))
    }
}

pub struct Object {
    object: MaybeUninit<libbpf_rs::OpenObject>,
    config: PerfCounterConfig,
}

impl Object {
    pub fn new(config: PerfCounterConfig) -> Self {
        Object {
            object: MaybeUninit::uninit(),
            config,
        }
    }

    pub fn build<'obj, F>(
        &'obj mut self,
        callback: F,
        stream_id: crate::StreamId,
    ) -> Result<PerfCounter<'obj, F>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) + 'obj,
    {
        PerfCounter::new(&mut self.object, callback, self.config.clone(), stream_id)
    }
}

pub struct PerfCounter<'obj, F> {
    _skel: PerfcounterSkel<'obj>,
    rb: RingBuffer<'obj>,
    _links: Vec<libbpf_rs::Link>,
    _perf_fds: Vec<Vec<RawFd>>,
    _phantom: PhantomData<F>,
}

impl<'obj, F> PerfCounter<'obj, F>
where
    F: for<'a> FnMut(Message<'a>) + 'obj,
{
    fn new(
        open_object: &'obj mut MaybeUninit<libbpf_rs::OpenObject>,
        mut callback: F,
        mut config: PerfCounterConfig,
        stream_id: crate::StreamId,
    ) -> Result<Self, BpfError> {
        let derived_info = config.validate()?;

        let skel_builder = PerfcounterSkelBuilder::default();
        let mut open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        open_skel
            .maps
            .events
            .set_max_entries(config.ringbuf as u32)
            .map_err(|e| BpfError::LoadError(format!("failed to set ring buffer size: {}", e)))?;

        let nprocs = libbpf_rs::num_possible_cpus()
            .map_err(|e| BpfError::LoadError(format!("failed to get cpu count: {}", e)))?;
        open_skel
            .maps
            .perf_counters
            .set_max_entries((nprocs * config.counters.len()) as u32)
            .map_err(|e| {
                BpfError::LoadError(format!("failed to set perf counters map size: {}", e))
            })?;

        open_skel
            .maps
            .rodata_data
            .as_mut()
            .unwrap()
            .cfg
            .counter_count = config.counters.len() as u32;

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {}", e)))?;

        let perf_type = libbpf_sys::PERF_TYPE_SOFTWARE;
        let perf_config = libbpf_sys::PERF_COUNT_SW_CPU_CLOCK;

        let timer_pefds = perf_event::perf_event_per_cpu(perf_type, perf_config, config.frequency)
            .map_err(|e| {
                BpfError::AttachError(format!("failed to create timer perf events: {}", e))
            })?;

        let links = perf_event::attach_perf_event(&timer_pefds, &mut skel.progs.perfcounter_timer)
            .map_err(|e| {
                BpfError::AttachError(format!("failed to attach timer perf event: {}", e))
            })?;

        let mut perf_fds = Vec::new();

        for (counter_idx, counter) in config.counters.iter().enumerate() {
            let (name, type_, perf_config) = counter.to_perf_config()?;
            tracing::debug!(
                "setting up counter idx={} name={} type={} config={:#x}",
                counter_idx,
                name,
                type_,
                perf_config
            );

            let mut counter_fds = Vec::new();
            for cpu in 0..nprocs {
                let fd = perf_event::perf_event_open(
                    type_,
                    perf_config as u32,
                    0,
                    None,
                    -1,
                    cpu as i32,
                    0,
                )
                .map_err(|e| {
                    BpfError::AttachError(format!(
                        "failed to open perf event for counter {} on cpu {}: {}",
                        name, cpu, e
                    ))
                })?;

                let idx = (cpu * config.counters.len() + counter_idx) as i32;
                let idx_bytes = idx.to_ne_bytes();
                let fd_bytes = fd.to_ne_bytes();
                skel.maps
                    .perf_counters
                    .update(&idx_bytes, &fd_bytes, libbpf_rs::MapFlags::ANY)
                    .map_err(|e| {
                        BpfError::MapError(format!(
                            "failed to update perf counter map at idx {}: {}",
                            idx, e
                        ))
                    })?;

                unsafe {
                    if libc::ioctl(fd, PERF_EVENT_IOC_ENABLE, 0) < 0 {
                        return Err(BpfError::AttachError(format!(
                            "failed to enable perf counter: {}",
                            std::io::Error::last_os_error()
                        )));
                    }
                }

                counter_fds.push(fd);
            }
            perf_fds.push(counter_fds);
        }

        for cpu in 0..nprocs {
            for (i, counter) in config.counters.iter().enumerate() {
                let counter_name = match counter {
                    CounterConfig::Named(name) => name.clone(),
                    CounterConfig::Custom { name, .. } => name.clone(),
                };

                let track_name = format!("{}/cpu{}", counter_name, cpu);
                let track_name_ref: &str = &track_name;

                let track = protocol::Track {
                    name: track_name_ref,
                    track_type: protocol::TrackType::Counter {
                        id: ((cpu as u64) << 32) | (i as u64),
                        unit: None,
                    },
                    parent: Some(protocol::TrackType::Cpu { cpu: cpu as u32 }),
                };

                callback(Message::Stream {
                    stream_id,
                    event: Event::Track(track),
                });
            }

            for (idx, (name, derived)) in derived_info.iter().enumerate() {
                if derived.is_some() {
                    let track_name = format!("{}/cpu{}", name, cpu);
                    let track_name_ref: &str = &track_name;

                    let track = protocol::Track {
                        name: track_name_ref,
                        track_type: protocol::TrackType::Counter {
                            id: ((cpu as u64) << 32) | ((config.counters.len() + idx) as u64),
                            unit: None,
                        },
                        parent: Some(protocol::TrackType::Cpu { cpu: cpu as u32 }),
                    };

                    callback(Message::Stream {
                        stream_id,
                        event: Event::Track(track),
                    });
                }
            }
        }

        let counters_len = config.counters.len();
        let derived_info_clone = derived_info.clone();

        let mut builder = RingBufferBuilder::new();
        builder
            .add(&skel.maps.events, move |data: &[u8]| {
                let event: &PerfCounterEvent = data.try_into().unwrap();

                for (i, &value) in event.counters[..counters_len].iter().enumerate() {
                    let counter = protocol::Counter {
                        name: "",
                        value: value as f64,
                        timestamp: event.timestamp,
                        track_id: protocol::TrackId::Counter {
                            id: ((event.cpu_id as u64) << 32) | (i as u64),
                        },
                        labels: Cow::Owned(protocol::Labels::new()),
                        unit: None,
                    };

                    callback(Message::Stream {
                        stream_id,
                        event: Event::Counter(counter),
                    });
                }

                for (idx, (_name, derived)) in derived_info_clone.iter().enumerate() {
                    if let Some(derived_counter) = derived {
                        match derived_counter {
                            DerivedCounter::Ipc {
                                cycles_idx,
                                instructions_idx,
                            } => {
                                let cycles = event.counters[*cycles_idx];
                                let instructions = event.counters[*instructions_idx];
                                let ipc = if cycles > 0 {
                                    instructions as f64 / cycles as f64
                                } else {
                                    0.0
                                };

                                let counter = protocol::Counter {
                                    name: "",
                                    value: ipc,
                                    timestamp: event.timestamp,
                                    track_id: protocol::TrackId::Counter {
                                        id: ((event.cpu_id as u64) << 32)
                                            | ((counters_len + idx) as u64),
                                    },
                                    labels: Cow::Owned(protocol::Labels::new()),
                                    unit: None,
                                };

                                callback(Message::Stream {
                                    stream_id,
                                    event: Event::Counter(counter),
                                });
                            }
                        }
                    }
                }
                0
            })
            .map_err(|e| BpfError::LoadError(format!("failed to add ring buffer: {}", e)))?;

        let rb = builder
            .build()
            .map_err(|e| BpfError::LoadError(format!("failed to build ring buffer: {}", e)))?;

        Ok(PerfCounter {
            _skel: skel,
            rb,
            _links: links,
            _perf_fds: perf_fds,
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

impl<'obj, F> Filterable for PerfCounter<'obj, F>
where
    F: for<'a> FnMut(Message<'a>) + 'obj,
{
    fn filter(&mut self, _pid: i32) -> Result<(), BpfError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation() {
        let mut config = PerfCounterConfig::default();
        assert!(config.validate().is_err());

        config
            .counters
            .push(CounterConfig::Named("cpu-cycles".to_string()));
        let result = config.validate();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);

        let mut config2 = PerfCounterConfig::default();
        config2
            .counters
            .push(CounterConfig::Named("unknown-counter".to_string()));
        assert!(config2.validate().is_err());

        let mut config3 = PerfCounterConfig::default();
        config3.counters.push(CounterConfig::Custom {
            name: "my-custom-counter".to_string(),
            perf_type: libbpf_sys::PERF_TYPE_RAW,
            config: 0x1234,
        });
        assert!(config3.validate().is_ok());

        let mut config4 = PerfCounterConfig::default();
        for _i in 0..=MAX_PERF_COUNTERS {
            config4
                .counters
                .push(CounterConfig::Named("cpu-cycles".to_string()));
        }
        assert!(config4.validate().is_err());
    }

    #[test]
    fn test_ipc_expansion() {
        let mut config = PerfCounterConfig::default();
        config
            .counters
            .push(CounterConfig::Named("ipc".to_string()));

        assert_eq!(config.counters.len(), 1);
        let derived_info = config.expand_counters().unwrap();
        assert_eq!(config.counters.len(), 2);
        assert_eq!(derived_info.len(), 1);

        match &config.counters[0] {
            CounterConfig::Named(name) => assert_eq!(name, "cpu-cycles"),
            _ => panic!("Expected Named variant"),
        }
        match &config.counters[1] {
            CounterConfig::Named(name) => assert_eq!(name, "cpu-instructions"),
            _ => panic!("Expected Named variant"),
        }

        let (name, derived) = &derived_info[0];
        assert_eq!(name, "ipc");
        match derived {
            Some(DerivedCounter::Ipc {
                cycles_idx,
                instructions_idx,
            }) => {
                assert_eq!(*cycles_idx, 0);
                assert_eq!(*instructions_idx, 1);
            }
            _ => panic!("Expected Ipc derived counter"),
        }
    }

    #[test]
    fn test_counter_to_perf_config() {
        let counter = CounterConfig::Named("cpu-cycles".to_string());
        let (name, perf_type, config) = counter.to_perf_config().unwrap();
        assert_eq!(name, "cpu-cycles");
        assert_eq!(perf_type, libbpf_sys::PERF_TYPE_HARDWARE);
        assert_eq!(config, libbpf_sys::PERF_COUNT_HW_CPU_CYCLES as u64);

        let counter2 = CounterConfig::Named("page-faults".to_string());
        let (name, perf_type, config) = counter2.to_perf_config().unwrap();
        assert_eq!(name, "page-faults");
        assert_eq!(perf_type, libbpf_sys::PERF_TYPE_SOFTWARE);
        assert_eq!(config, libbpf_sys::PERF_COUNT_SW_PAGE_FAULTS as u64);
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    #[ignore = "requires root"]
    fn test_perfcounter_page_faults() {
        assert!(is_root());

        let config = PerfCounterConfig {
            frequency: 99,
            counters: vec![CounterConfig::Named("page-faults".to_string())],
            pid_filters: vec![std::process::id() as i32],
            filter_process: vec![],
            ringbuf: default_ringbuf_size(),
        };

        let counter_data = Arc::new(Mutex::new(HashMap::new()));
        let counter_data_ref = counter_data.clone();
        let track_count = Arc::new(Mutex::new(0));
        let track_count_ref = track_count.clone();

        let mut object = Object::new(config);
        let mut perfcounter = object
            .build(
                move |message| match message {
                    Message::Stream {
                        event: Event::Counter(counter),
                        ..
                    } => {
                        let mut data = counter_data_ref.lock().unwrap();
                        let key = (counter.track_id, counter.timestamp);
                        data.insert(key, counter.value);
                    }
                    Message::Stream {
                        event: Event::Track(_),
                        ..
                    } => {
                        let mut count = track_count_ref.lock().unwrap();
                        *count += 1;
                    }
                    _ => {}
                },
                0,
            )
            .expect("failed to create perfcounter");

        let test_thread = thread::spawn(|| {
            let mut allocations = Vec::new();
            for i in 0..20 {
                let size = 4 * 1024 * 1024;
                let mut vec: Vec<u8> = vec![0; size];
                for j in (0..size).step_by(4096) {
                    vec[j] = (i + j) as u8;
                }

                allocations.push(vec);
                thread::sleep(Duration::from_millis(20));
            }

            allocations
        });

        thread::sleep(Duration::from_millis(500));
        for _ in 0..20 {
            perfcounter.consume().unwrap();
            thread::sleep(Duration::from_millis(20));
        }

        let _result = test_thread.join().unwrap();

        let tracks = track_count.lock().unwrap();
        let ncpus = libbpf_rs::num_possible_cpus().unwrap();
        let expected_tracks = ncpus;
        assert_eq!(
            *tracks, expected_tracks,
            "should have created {} tracks",
            expected_tracks
        );

        let data = counter_data.lock().unwrap();
        assert!(!data.is_empty(), "should have collected counter data");
        let mut page_fault_count = 0;

        for (_, value) in data.iter() {
            if *value > 0.0 {
                page_fault_count += 1;
            }
        }

        assert!(
            page_fault_count > 0,
            "should have collected page fault data, got {} samples",
            data.len()
        );
    }
}
