mod cpuutil_skel {
    include!(concat!(env!("OUT_DIR"), "/cpuutil.skel.rs"));
}

use cpuutil_skel::*;

use crate::{perf_event, BpfError};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{set_print, MapCore, MapFlags, OpenObject, PrintLevel, RingBufferBuilder};
use libbpf_sys::{PERF_COUNT_SW_CPU_CLOCK, PERF_TYPE_SOFTWARE};
use protocol::{Counter, Event, Labels};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::mem::MaybeUninit;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuUtilConfig {
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    #[serde(default)]
    pub pid_filters: Vec<u32>,
}

impl Default for CpuUtilConfig {
    fn default() -> Self {
        Self {
            interval_ms: default_interval_ms(),
            pid_filters: Vec::new(),
        }
    }
}

fn default_interval_ms() -> u64 {
    1000
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
        F: for<'a> FnMut(Event<'a>) + 'bd,
    {
        let mut util =
            CpuUtil::new_with_debug(&mut self.object, callback, self.config.interval_ms, false)?;

        for pid in &self.config.pid_filters {
            util.add_pid_filter(*pid)?;
        }

        Ok(util)
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct CpuEvent {
    pub tid: u32,
    pub tgid: u32,
    pub total_time_ns: u64,
    pub kernel_time_ns: u64,
    pub cpu: u32,
    pub timestamp: u64,
}

unsafe impl plain::Plain for CpuEvent {}

impl<'a> TryFrom<&'a [u8]> for &'a CpuEvent {
    type Error = BpfError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        plain::from_bytes(data)
            .map_err(|e| BpfError::MapError(format!("failed to parse cpu event: {:?}", e)))
    }
}

#[derive(Default)]
struct ThreadStats {
    cpu_time_ns: u64,
    kernel_time_ns: u64,
    min_timestamp: Option<u64>,
}

pub struct CpuUtil<'this, F>
{
    _skel: CpuutilSkel<'this>,
    ringbuf: libbpf_rs::RingBuffer<'this>,
    callback: Rc<RefCell<F>>,
    interval: Duration,
    last_report: Instant,
    thread_stats: Rc<RefCell<HashMap<(i32, i32), ThreadStats>>>, // (pid, tid) -> stats
    min_cpu_timestamps: Rc<RefCell<HashMap<u32, u64>>>,          // cpu -> min timestamp
    _perf_links: Option<Vec<libbpf_rs::Link>>,
}

impl<'this, F> CpuUtil<'this, F>
where
    F: for<'a> FnMut(Event<'a>) + 'this,
{
    fn new_with_debug(
        open_object: &'this mut MaybeUninit<OpenObject>,
        callback: F,
        interval_ms: u64,
        debug: bool,
    ) -> Result<Self, BpfError> {
        if !debug {
            set_print(Some((PrintLevel::Debug, |_level, _msg| {})));
        }

        let skel_builder = CpuutilSkelBuilder::default();

        let open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {}", e)))?;

        skel.attach()
            .map_err(|e| BpfError::AttachError(format!("failed to attach bpf programs: {}", e)))?;

        let thread_stats: Rc<RefCell<HashMap<(i32, i32), ThreadStats>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let thread_stats_clone = thread_stats.clone();
        let min_cpu_timestamps: Rc<RefCell<HashMap<u32, u64>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let min_cpu_timestamps_clone = min_cpu_timestamps.clone();
        let callback_rc = Rc::new(RefCell::new(callback));

        let mut builder = RingBufferBuilder::new();
        builder
            .add(&skel.maps.events, move |data| {
                let cpu_event: &CpuEvent = data.try_into().unwrap();

                let mut stats = thread_stats_clone.borrow_mut();
                let key = (cpu_event.tgid as i32, cpu_event.tid as i32);
                let entry: &mut ThreadStats = stats.entry(key).or_default();
                entry.cpu_time_ns += cpu_event.total_time_ns;
                entry.kernel_time_ns += cpu_event.kernel_time_ns;
                entry.min_timestamp = match entry.min_timestamp {
                    Some(ts) => Some(ts.min(cpu_event.timestamp)),
                    None => Some(cpu_event.timestamp),
                };

                let mut cpu_timestamps = min_cpu_timestamps_clone.borrow_mut();
                cpu_timestamps
                    .entry(cpu_event.cpu)
                    .and_modify(|ts| *ts = (*ts).min(cpu_event.timestamp))
                    .or_insert(cpu_event.timestamp);

                0
            })
            .map_err(|e| BpfError::MapError(format!("failed to add ring buffer: {}", e)))?;

        let ringbuf = builder
            .build()
            .map_err(|e| BpfError::MapError(format!("failed to build ring buffer: {}", e)))?;

        let perf_links = if interval_ms > 0 {
            let freq = 1000 / interval_ms;
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

            Some(links)
        } else {
            None
        };

        Ok(CpuUtil {
            _skel: skel,
            ringbuf,
            callback: callback_rc,
            interval: Duration::from_millis(interval_ms),
            last_report: Instant::now(),
            thread_stats,
            min_cpu_timestamps,
            _perf_links: perf_links,
        })
    }

    pub fn track_tgid(&mut self, tgid: u32) -> Result<(), BpfError> {
        let one: u32 = 1;
        let tgid_bytes = tgid.to_ne_bytes();
        let one_bytes = one.to_ne_bytes();
        self._skel
            .maps
            .tracked_tgids
            .update(&tgid_bytes, &one_bytes, MapFlags::ANY)
            .map_err(|e| BpfError::MapError(format!("failed to track tgid {}: {}", tgid, e)))?;
        Ok(())
    }

    pub fn untrack_tgid(&mut self, tgid: u32) -> Result<(), BpfError> {
        let tgid_bytes = tgid.to_ne_bytes();
        self._skel
            .maps
            .tracked_tgids
            .delete(&tgid_bytes)
            .map_err(|e| BpfError::MapError(format!("failed to untrack tgid {}: {}", tgid, e)))?;
        Ok(())
    }

    pub fn poll(&mut self, timeout: Duration) -> Result<(), BpfError> {
        self.ringbuf
            .poll(timeout)
            .map_err(|e| BpfError::MapError(format!("failed to poll ring buffer: {}", e)))?;

        let now = Instant::now();
        if now.duration_since(self.last_report) >= self.interval {
            self.report_interval_stats();
            self.last_report = now;
        }

        Ok(())
    }

    fn report_interval_stats(&mut self) {
        let mut stats = self.thread_stats.borrow_mut();
        let mut callback = self.callback.borrow_mut();
        let mut cpu_timestamps = self.min_cpu_timestamps.borrow_mut();

        let min_timestamp = cpu_timestamps.values().min().copied().unwrap_or(0);

        for ((pid, tid), thread_stats) in stats.drain() {
            if thread_stats.cpu_time_ns > 0 {
                let cpu_counter = Event::Counter(Counter {
                    name: "cpu_time_ns",
                    value: thread_stats.cpu_time_ns as f64,
                    timestamp: min_timestamp,
                    tid,
                    pid,
                    labels: Cow::Owned(Labels::new()),
                });
                callback(cpu_counter);
            }

            if thread_stats.kernel_time_ns > 0 {
                let kernel_counter = Event::Counter(Counter {
                    name: "kernel_time_ns",
                    value: thread_stats.kernel_time_ns as f64,
                    timestamp: min_timestamp,
                    tid,
                    pid,
                    labels: Cow::Owned(Labels::new()),
                });
                callback(kernel_counter);
            }
        }

        cpu_timestamps.clear();
    }

    pub fn add_pid_filter(&mut self, pid: u32) -> Result<(), BpfError> {
        self._skel
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

        let now = Instant::now();
        if now.duration_since(self.last_report) >= self.interval {
            self.report_interval_stats();
            self.last_report = now;
        }

        Ok(())
    }
}
