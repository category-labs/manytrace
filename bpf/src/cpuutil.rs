mod cpuutil_skel {
    include!(concat!(env!("OUT_DIR"), "/cpuutil.skel.rs"));
}

use cpuutil_skel::*;

use crate::{perf_event, BpfError, Filterable};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, OpenObject, RingBufferBuilder};
use libbpf_sys::{PERF_COUNT_SW_CPU_CLOCK, PERF_TYPE_SOFTWARE};
use protocol::{Counter, Event, Labels, Message};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::time::Duration;
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuUtilConfig {
    #[serde(default = "default_frequency")]
    pub frequency: u64,
    #[serde(default)]
    pub pid_filters: Vec<i32>,
    #[serde(default)]
    pub filter_process: Vec<String>,
}

fn default_frequency() -> u64 {
    9
}

pub struct Object {
    object: MaybeUninit<libbpf_rs::OpenObject>,
    config: CpuUtilConfig,
}

impl Object {
    pub fn new(config: CpuUtilConfig) -> Self {
        Self {
            object: MaybeUninit::uninit(),
            config,
        }
    }

    pub fn build<'bd, F>(&'bd mut self, callback: F) -> Result<CpuUtil<'bd, F>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) + 'bd,
    {
        let util = CpuUtil::new(
            &mut self.object,
            self.config.clone(),
            callback,
            self.config.frequency,
        )?;
        Ok(util)
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct CpuEvent {
    pub tid: u32,
    pub tgid: u32,
    pub end_time: u64,
    pub kernel_time_ns: u64,
    pub start_time: u64,
    pub cpu: u32,
    pub boundary: bool,
}

unsafe impl plain::Plain for CpuEvent {}

impl<'a> TryFrom<&'a [u8]> for &'a CpuEvent {
    type Error = BpfError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        plain::from_bytes(data)
            .map_err(|e| BpfError::MapError(format!("failed to parse cpu event: {:?}", e)))
    }
}

struct ThreadStats {
    cpu_time_ns: u64,
    kernel_time_ns: u64,
    min_timestamp: u64,
}

impl Default for ThreadStats {
    fn default() -> Self {
        Self {
            cpu_time_ns: 0,
            kernel_time_ns: 0,
            min_timestamp: u64::MAX,
        }
    }
}

pub struct CpuUtil<'this, F> {
    skel: CpuutilSkel<'this>,
    ringbuf: libbpf_rs::RingBuffer<'this>,
    _perf_links: Vec<libbpf_rs::Link>,
    _phantom: PhantomData<F>,
}

impl<'this, F> CpuUtil<'this, F>
where
    F: for<'a> FnMut(Message<'a>) + 'this,
{
    fn new(
        open_object: &'this mut MaybeUninit<OpenObject>,
        config: CpuUtilConfig,
        mut callback: F,
        freq: u64,
    ) -> Result<Self, BpfError> {
        let skel_builder = CpuutilSkelBuilder::default();

        let mut open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        // Set filter_enabled based on whether we have any filters
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

        let mut thread_stats: HashMap<(i32, i32), ThreadStats> = HashMap::new();
        let mut builder = RingBufferBuilder::new();
        let nprocs = libbpf_rs::num_possible_cpus().unwrap();
        let mut boundaries_reported = 0;
        let mut max_timestamp = 0;
        builder
            .add(&skel.maps.events, move |data| {
                let cpu_event: &CpuEvent = data.try_into().unwrap();
                if cpu_event.tgid != 0 && cpu_event.start_time != 0 {
                    let key = (cpu_event.tgid as i32, cpu_event.tid as i32);
                    let entry: &mut ThreadStats = thread_stats.entry(key).or_default();
                    entry.cpu_time_ns += cpu_event.end_time - cpu_event.start_time;
                    entry.kernel_time_ns += cpu_event.kernel_time_ns;
                    entry.min_timestamp = entry.min_timestamp.min(cpu_event.start_time);
                }
                max_timestamp = max_timestamp.max(cpu_event.end_time);
                if cpu_event.boundary {
                    boundaries_reported += 1;
                    if boundaries_reported == nprocs {
                        boundaries_reported = 0;
                        for ((pid, tid), thread_stats) in thread_stats.iter_mut() {
                            let elapsed_ns = (max_timestamp - thread_stats.min_timestamp) as f64;
                            let cpu_percent =
                                (thread_stats.cpu_time_ns as f64 / elapsed_ns) * 100.0;
                            debug!(
                                pid = pid,
                                tid = tid,
                                value = cpu_percent,
                                cpu_time_ns = thread_stats.cpu_time_ns,
                                elapsed_ns = elapsed_ns as u64,
                                "emitting cpu_time counter"
                            );
                            let cpu_counter = Message::Event(Event::Counter(Counter {
                                name: "cpu_time",
                                value: cpu_percent,
                                timestamp: thread_stats.min_timestamp,
                                tid: *tid,
                                pid: *pid,
                                labels: Cow::Owned(Labels::new()),
                                unit: Some("%"),
                            }));
                            callback(cpu_counter);

                            let kernel_percent =
                                (thread_stats.kernel_time_ns as f64 / elapsed_ns) * 100.0;
                            debug!(
                                pid = pid,
                                tid = tid,
                                value = kernel_percent,
                                kernel_time_ns = thread_stats.kernel_time_ns,
                                elapsed_ns = elapsed_ns as u64,
                                "emitting kernel_time counter"
                            );
                            let kernel_counter = Message::Event(Event::Counter(Counter {
                                name: "kernel_time",
                                value: kernel_percent,
                                timestamp: thread_stats.min_timestamp,
                                tid: *tid,
                                pid: *pid,
                                labels: Cow::Owned(Labels::new()),
                                unit: Some("%"),
                            }));
                            callback(kernel_counter);
                            thread_stats.cpu_time_ns = 0;
                            thread_stats.kernel_time_ns = 0;
                            thread_stats.min_timestamp = max_timestamp;
                        }
                    }
                }

                0
            })
            .map_err(|e| BpfError::MapError(format!("failed to add ring buffer: {}", e)))?;

        let ringbuf = builder
            .build()
            .map_err(|e| BpfError::MapError(format!("failed to build ring buffer: {}", e)))?;

        let perf_links = {
            let perf_fds = perf_event::perf_event_per_cpu(
                PERF_TYPE_SOFTWARE,
                PERF_COUNT_SW_CPU_CLOCK,
                freq,
            )
            .map_err(|e| BpfError::LoadError(format!("failed to open perf events: {}", e)))?;

            let prog = &mut skel.progs.handle_boundary_event;
            let links = perf_event::attach_perf_event(&perf_fds, prog).map_err(|e| {
                BpfError::AttachError(format!("failed to attach perf events: {}", e))
            })?;

            links
        };

        Ok(CpuUtil {
            skel,
            ringbuf,
            _perf_links: perf_links,
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

impl<'this, F> Filterable for CpuUtil<'this, F>
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
    use rstest::*;
    use std::{cell::RefCell, io::Write, rc::Rc, time::Instant};
    use tempfile::NamedTempFile;

    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[derive(Debug, Clone)]
    struct TestCounter {
        name: String,
        value: f64,
        #[allow(dead_code)]
        timestamp: u64,
        #[allow(dead_code)]
        tid: i32,
        pid: i32,
    }

    struct CpuUtilFixture {
        events: Rc<RefCell<Vec<TestCounter>>>,
        current_pid: u32,
    }

    #[fixture]
    fn cpuutil_setup() -> CpuUtilFixture {
        let current_pid = std::process::id();
        let events = Rc::new(RefCell::new(Vec::new()));

        CpuUtilFixture {
            events,
            current_pid,
        }
    }

    #[rstest]
    #[ignore = "requires root"]
    fn test_cpuutil_events(cpuutil_setup: CpuUtilFixture) {
        assert!(is_root());

        let events_clone = cpuutil_setup.events.clone();

        let config = CpuUtilConfig {
            frequency: 100,
            pid_filters: vec![cpuutil_setup.current_pid as i32],
            filter_process: vec![],
        };

        let mut object = Object::new(config);
        let mut cpuutil = object
            .build(move |message| {
                if let Message::Event(Event::Counter(c)) = message {
                    let test_counter = TestCounter {
                        name: c.name.to_string(),
                        value: c.value,
                        timestamp: c.timestamp,
                        tid: c.tid,
                        pid: c.pid,
                    };
                    events_clone.borrow_mut().push(test_counter);
                }
            })
            .expect("failed to build cpu util");

        let start = Instant::now();
        let mut primes = vec![];
        while start.elapsed() < Duration::from_millis(300) {
            for num in 2..5000 {
                let mut is_prime = true;
                for i in 2..num {
                    if num % i == 0 {
                        is_prime = false;
                        break;
                    }
                }
                if is_prime {
                    primes.push(num);
                }
            }

            let _ = cpuutil.consume();
        }

        let _ = cpuutil.consume();
        let collected_events = cpuutil_setup.events.borrow();
        assert!(!collected_events.is_empty(), "no events were captured");
        let cpu_time_events: Vec<_> = collected_events
            .iter()
            .filter(|c| c.name == "cpu_time" && c.pid == cpuutil_setup.current_pid as i32)
            .collect();
        assert!(
            !cpu_time_events.is_empty(),
            "no cpu_time events for current process"
        );

        for event in &cpu_time_events {
            assert!(event.value > 0.0, "cpu time should be greater than zero");
            assert!(
                event.value <= 100.0,
                "cpu time percentage should not exceed 100%"
            );
        }
    }

    #[rstest]
    #[ignore = "requires root"]
    fn test_cpuutil_kernel_time(cpuutil_setup: CpuUtilFixture) {
        assert!(is_root());

        let events_clone = cpuutil_setup.events.clone();

        let config = CpuUtilConfig {
            frequency: 100,
            pid_filters: vec![cpuutil_setup.current_pid as i32],
            filter_process: vec![],
        };

        let mut object = Object::new(config);
        let mut cpuutil = object
            .build(move |message| {
                if let Message::Event(Event::Counter(c)) = message {
                    let test_counter = TestCounter {
                        name: c.name.to_string(),
                        value: c.value,
                        timestamp: c.timestamp,
                        tid: c.tid,
                        pid: c.pid,
                    };
                    events_clone.borrow_mut().push(test_counter);
                }
            })
            .expect("failed to build cpu util");

        let start = Instant::now();
        let mut temp_file = NamedTempFile::new().expect("failed to create temp file");
        let data = vec![0u8; 1024 * 1024]; // 1MB of data

        while start.elapsed() < Duration::from_millis(300) {
            for _ in 0..10 {
                temp_file
                    .write_all(&data)
                    .expect("failed to write to temp file");
                temp_file.flush().expect("failed to flush temp file");
                temp_file
                    .as_file()
                    .sync_data()
                    .expect("failed to sync temp file");
            }

            let _ = cpuutil.consume();
        }

        let _ = cpuutil.consume();
        let collected_events = cpuutil_setup.events.borrow();
        assert!(!collected_events.is_empty(), "no events were captured");

        let kernel_time_events: Vec<_> = collected_events
            .iter()
            .filter(|c| c.name == "kernel_time" && c.pid == cpuutil_setup.current_pid as i32)
            .collect();

        let cpu_time_events: Vec<_> = collected_events
            .iter()
            .filter(|c| c.name == "cpu_time" && c.pid == cpuutil_setup.current_pid as i32)
            .collect();

        assert!(
            !cpu_time_events.is_empty() || !kernel_time_events.is_empty(),
            "should have at least cpu_time or kernel_time events"
        );
    }
}
