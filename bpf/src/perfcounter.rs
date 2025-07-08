use crate::{BpfError, Filterable};
use libbpf_rs::RingBuffer;
use libbpf_sys;
use protocol::Message;
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::time::Duration;
use std::{convert::TryFrom, os::fd::RawFd};

pub const MAX_PERF_COUNTERS: usize = 8;

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

// mod perfcounter_bpf {
//     include!(concat!(env!("OUT_DIR"), "/perfcounter.skel.rs"));
// }
// use perfcounter_bpf::*;
#[allow(dead_code)]
struct PerfcounterSkel<'a> {
    _phantom: PhantomData<&'a ()>,
}

#[allow(dead_code)]
struct PerfcounterSkelBuilder;

impl Default for PerfcounterSkelBuilder {
    fn default() -> Self {
        Self
    }
}

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
        _open_object: &'obj mut MaybeUninit<libbpf_rs::OpenObject>,
        _callback: F,
        mut config: PerfCounterConfig,
        _stream_id: crate::StreamId,
    ) -> Result<Self, BpfError> {
        config.validate()?;

        Err(BpfError::LoadError(
            "PerfCounter BPF program not yet implemented".to_string(),
        ))
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
