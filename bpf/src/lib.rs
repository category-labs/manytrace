use blazesym::symbolize::Symbolizer;
use protocol::{Message, StreamId, StreamIdAllocator};
use serde::{Deserialize, Serialize};
use std::{cell::RefCell, collections::HashSet, path::Path, rc::Rc};
use thiserror::Error;
use tracing::debug;

pub use protocol::Message as BpfMessage;

pub mod cpuutil;
mod perf_event;
pub mod perfcounter;
pub mod profiler;
pub mod schedtrace;
pub mod threadtrack;

pub(crate) trait Filterable {
    fn filter(&mut self, pid: i32) -> Result<(), BpfError>;
}

fn get_monotonic_timestamp() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub use cpuutil::CpuUtilConfig;
pub use perfcounter::PerfCounterConfig;
pub use profiler::ProfilerConfig;
pub use schedtrace::SchedTraceConfig;
pub use threadtrack::ThreadTrackerConfig;

#[derive(Error, Debug)]
pub enum BpfError {
    #[error("failed to load BPF program: {0}")]
    LoadError(String),
    #[error("failed to attach BPF program: {0}")]
    AttachError(String),
    #[error("BPF map operation failed: {0}")]
    MapError(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BpfConfig {
    #[serde(default)]
    pub thread_tracker: Option<ThreadTrackerConfig>,
    #[serde(default)]
    pub cpu_util: Option<CpuUtilConfig>,
    #[serde(default)]
    pub profiler: Option<ProfilerConfig>,
    #[serde(default)]
    pub schedtrace: Option<SchedTraceConfig>,
    #[serde(default)]
    pub perfcounter: Option<PerfCounterConfig>,
    #[serde(default)]
    pub filter_process: Vec<String>,
}

impl BpfConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, BpfError> {
        let path = path.as_ref();
        debug!(path = %path.display(), "loading bpf config");

        let content = std::fs::read_to_string(path)
            .map_err(|e| BpfError::LoadError(format!("failed to read config file: {}", e)))?;

        let config = toml::from_str(&content)
            .map_err(|e| BpfError::LoadError(format!("failed to parse TOML: {}", e)))?;

        Ok(config)
    }

    pub fn from_toml_str(content: &str) -> Result<Self, BpfError> {
        let config = toml::from_str(content)
            .map_err(|e| BpfError::LoadError(format!("failed to parse TOML: {}", e)))?;

        Ok(config)
    }

    pub fn build(self) -> Result<BpfObject, BpfError> {
        let global_filter = if !self.filter_process.is_empty() {
            Some(self.filter_process.clone())
        } else {
            None
        };

        let needs_process_filtering = {
            let cpu_needs = self
                .cpu_util
                .as_ref()
                .map(|cfg| !cfg.filter_process.is_empty() || global_filter.is_some())
                .unwrap_or(false);
            let profiler_needs = self
                .profiler
                .as_ref()
                .map(|cfg| !cfg.filter_process.is_empty() || global_filter.is_some())
                .unwrap_or(false);
            let schedtrace_needs = self
                .schedtrace
                .as_ref()
                .map(|cfg| !cfg.filter_process.is_empty() || global_filter.is_some())
                .unwrap_or(false);
            cpu_needs || profiler_needs || schedtrace_needs
        };

        if needs_process_filtering && self.thread_tracker.is_none() {
            return Err(BpfError::LoadError(
                "Thread tracker must be enabled when using process name filtering".to_string(),
            ));
        }

        let threadtrack = if let Some(cfg) = self.thread_tracker {
            debug!("initializing thread tracker");
            Some(threadtrack::Object::new(cfg))
        } else {
            None
        };

        let (cpuutils, cpuutil_filters) = if let Some(mut cfg) = self.cpu_util {
            if let Some(ref global) = global_filter {
                cfg.filter_process = global.clone();
            }

            debug!(
                module = "cpuutil",
                frequency = cfg.frequency,
                pid_filters = ?cfg.pid_filters,
                filter_process = ?cfg.filter_process,
                "initializing cpu utilization monitor"
            );
            let filters: HashSet<String> = cfg.filter_process.iter().cloned().collect();
            (Some(cpuutil::Object::new(cfg)), filters)
        } else {
            (None, HashSet::new())
        };

        let (profiler, profiler_filters) = if let Some(mut cfg) = self.profiler {
            if let Some(ref global) = global_filter {
                cfg.filter_process = global.clone();
            }

            debug!(
                module = "profiler",
                frequency = cfg.frequency,
                kernel_samples = cfg.kernel_samples,
                user_samples = cfg.user_samples,
                pid_filters = ?cfg.pid_filters,
                filter_process = ?cfg.filter_process,
                "initializing profiler"
            );
            let filters: HashSet<String> = cfg.filter_process.iter().cloned().collect();
            (Some(profiler::Object::new(cfg)), filters)
        } else {
            (None, HashSet::new())
        };

        let (schedtrace, schedtrace_filters) = if let Some(mut cfg) = self.schedtrace {
            if let Some(ref global) = global_filter {
                cfg.filter_process = global.clone();
            }

            debug!(
                module = "schedtrace",
                pid_filters = ?cfg.pid_filters,
                filter_process = ?cfg.filter_process,
                "initializing scheduler tracer"
            );
            let filters: HashSet<String> = cfg.filter_process.iter().cloned().collect();
            (Some(schedtrace::Object::new(cfg)), filters)
        } else {
            (None, HashSet::new())
        };

        let perfcounter = if let Some(cfg) = self.perfcounter {
            debug!(
                module = "perfcounter",
                frequency = cfg.frequency,
                counters = ?cfg.counters,
                "initializing performance counter"
            );
            Some(perfcounter::Object::new(cfg))
        } else {
            None
        };

        Ok(BpfObject {
            symbolizer: Symbolizer::new(),
            threadtrack,
            cpuutils,
            profiler,
            schedtrace,
            perfcounter,
            cpuutil_filters,
            profiler_filters,
            schedtrace_filters,
            watched_tgids: HashSet::new(),
        })
    }
}

pub struct BpfObject {
    symbolizer: Symbolizer,
    threadtrack: Option<threadtrack::Object>,
    cpuutils: Option<cpuutil::Object>,
    profiler: Option<profiler::Object>,
    schedtrace: Option<schedtrace::Object>,
    perfcounter: Option<perfcounter::Object>,
    cpuutil_filters: HashSet<String>,
    profiler_filters: HashSet<String>,
    schedtrace_filters: HashSet<String>,
    watched_tgids: HashSet<i32>,
}

impl BpfObject {
    pub fn consumer<'this, F>(
        &'this mut self,
        callback: F,
        stream_allocator: &mut StreamIdAllocator,
    ) -> Result<BpfConsumer<'this>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) -> i32 + Clone + 'this,
    {
        let cpuutil = if let Some(ref mut obj) = self.cpuutils {
            Some(obj.build(
                Box::new(callback.clone()) as Box<dyn for<'a> FnMut(Message<'a>) -> i32>,
            )?)
        } else {
            None
        };

        let profiler = if let Some(ref mut obj) = self.profiler {
            let callback = Box::new(callback.clone()) as Box<dyn for<'a> FnMut(Message<'a>) -> i32>;
            let stream_id = stream_allocator.allocate();
            Some(obj.build(callback, &self.symbolizer, stream_id)?)
        } else {
            None
        };

        let schedtrace = if let Some(ref mut obj) = self.schedtrace {
            Some(obj.build(
                Box::new(callback.clone()) as Box<dyn for<'a> FnMut(Message<'a>) -> i32>,
                &self.symbolizer,
            )?)
        } else {
            None
        };

        let perfcounter = if let Some(ref mut obj) = self.perfcounter {
            let callback = Box::new(callback.clone()) as Box<dyn for<'a> FnMut(Message<'a>) -> i32>;
            let stream_id = stream_allocator.allocate();
            Some(obj.build(callback, stream_id)?)
        } else {
            None
        };

        let cpuutil_rc = cpuutil.map(|c| Rc::new(RefCell::new(c)));
        let profiler_rc = profiler.map(|p| Rc::new(RefCell::new(p)));
        let schedtrace_rc = schedtrace.map(|s| Rc::new(RefCell::new(s)));
        let perfcounter_rc = perfcounter.map(|p| Rc::new(RefCell::new(p)));

        let threadtrack = if let Some(ref mut obj) = self.threadtrack {
            let has_cpuutil = cpuutil_rc.is_some();
            let has_profiler = profiler_rc.is_some();
            let has_schedtrace = schedtrace_rc.is_some();

            if has_cpuutil || has_profiler || has_schedtrace {
                let mut user_callback = callback.clone();
                let cpuutil_filters = self.cpuutil_filters.clone();
                let profiler_filters = self.profiler_filters.clone();
                let schedtrace_filters = self.schedtrace_filters.clone();
                let cpuutil_ref = cpuutil_rc.clone();
                let profiler_ref = profiler_rc.clone();
                let schedtrace_ref = schedtrace_rc.clone();
                let mut watched_tgids = std::mem::take(&mut self.watched_tgids);

                let wrapper_callback = move |message: Message<'_>| -> i32 {
                    let mut should_emit_discovered = false;
                    let mut thread_tid = 0;
                    let mut thread_pid = 0;

                    if let Message::Event(protocol::Event::Track(ref track)) = message {
                        match track.track_type {
                            protocol::TrackType::Process { pid } => {
                                let name = track.name;

                                if let Some(ref cpu) = cpuutil_ref {
                                    let base_name = name.split('/').next_back().unwrap_or(name);
                                    if cpuutil_filters.contains(name)
                                        || cpuutil_filters.contains(base_name)
                                    {
                                        if let Err(e) = cpu.borrow_mut().filter(pid) {
                                            tracing::warn!(
                                            "Failed to add process {} (pid {}) to cpuutil filter: {}",
                                            name,
                                            pid,
                                            e
                                        );
                                        } else {
                                            watched_tgids.insert(pid);
                                        }
                                    }
                                }
                                if let Some(ref cpu) = profiler_ref {
                                    let base_name = name.split('/').next_back().unwrap_or(name);
                                    if profiler_filters.contains(name)
                                        || profiler_filters.contains(base_name)
                                    {
                                        if let Err(e) = cpu.borrow_mut().filter(pid) {
                                            tracing::warn!(
                                            "Failed to add process {} (pid {}) to profiler filter: {}",
                                            name,
                                            pid,
                                            e
                                        );
                                        } else {
                                            watched_tgids.insert(pid);
                                        }
                                    }
                                }
                                if let Some(ref sched) = schedtrace_ref {
                                    let base_name = name.split('/').next_back().unwrap_or(name);
                                    if schedtrace_filters.contains(name)
                                        || schedtrace_filters.contains(base_name)
                                    {
                                        if let Err(e) = sched.borrow_mut().filter(pid) {
                                            tracing::warn!(
                                            "Failed to add process {} (pid {}) to schedtrace filter: {}",
                                            name,
                                            pid,
                                            e
                                        );
                                        } else {
                                            watched_tgids.insert(pid);
                                        }
                                    }
                                }
                            }
                            protocol::TrackType::Thread { tid, pid } => {
                                if watched_tgids.contains(&pid) {
                                    should_emit_discovered = true;
                                    thread_tid = tid;
                                    thread_pid = pid;
                                }
                            }
                            _ => {}
                        }
                    }

                    let result = user_callback(message);
                    if result != 0 {
                        return result;
                    }

                    if should_emit_discovered {
                        let instant_event =
                            Message::Event(protocol::Event::Instant(protocol::Instant {
                                name: "discovered",
                                timestamp: get_monotonic_timestamp(),
                                track_id: protocol::TrackId::Thread {
                                    tid: thread_tid,
                                    pid: thread_pid,
                                },
                                labels: std::borrow::Cow::Owned(protocol::Labels::new()),
                            }));
                        return user_callback(instant_event);
                    }

                    result
                };

                let callback =
                    Box::new(wrapper_callback) as Box<dyn for<'a> FnMut(Message<'a>) -> i32>;
                Some(obj.build(callback)?)
            } else {
                let callback = Box::new(callback) as Box<dyn for<'a> FnMut(Message<'a>) -> i32>;
                Some(obj.build(callback)?)
            }
        } else {
            None
        };

        Ok(BpfConsumer {
            threadtrack,
            cpuutil: cpuutil_rc,
            profiler: profiler_rc,
            schedtrace: schedtrace_rc,
            perfcounter: perfcounter_rc,
        })
    }
}

type Callback<'cb> = Box<dyn for<'a> FnMut(Message<'a>) -> i32 + 'cb>;

pub struct BpfConsumer<'this> {
    threadtrack: Option<threadtrack::ThreadTracker<'this, Callback<'this>>>,
    cpuutil: Option<Rc<RefCell<cpuutil::CpuUtil<'this, Callback<'this>>>>>,
    profiler: Option<Rc<RefCell<profiler::Profiler<'this, Callback<'this>>>>>,
    schedtrace: Option<Rc<RefCell<schedtrace::SchedTrace<'this, Callback<'this>>>>>,
    perfcounter: Option<Rc<RefCell<perfcounter::PerfCounter<'this, Callback<'this>>>>>,
}

impl<'this> BpfConsumer<'this> {
    pub fn consume(&mut self) -> Result<(), BpfError> {
        if let Some(ref mut tracker) = self.threadtrack {
            tracker.consume()?;
        }
        if let Some(ref util) = self.cpuutil {
            util.borrow_mut().consume()?;
        }
        if let Some(ref prof) = self.profiler {
            prof.borrow_mut().consume()?;
        }
        if let Some(ref sched) = self.schedtrace {
            sched.borrow_mut().consume()?;
        }
        if let Some(ref perf) = self.perfcounter {
            perf.borrow_mut().consume()?;
        }
        Ok(())
    }

    pub fn poll(&mut self, timeout: std::time::Duration) -> Result<(), BpfError> {
        if let Some(ref mut tracker) = self.threadtrack {
            tracker.poll(timeout)?;
        }
        if let Some(ref util) = self.cpuutil {
            util.borrow_mut().poll(timeout)?;
        }
        if let Some(ref prof) = self.profiler {
            prof.borrow_mut().poll(timeout)?;
        }
        if let Some(ref sched) = self.schedtrace {
            sched.borrow_mut().poll(timeout)?;
        }
        if let Some(ref perf) = self.perfcounter {
            perf.borrow_mut().poll(timeout)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_parsing() {
        let config_str = r#"
[thread_tracker]

[cpu_util]
frequency = 2000
pid_filters = [1234, 5678]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        assert!(config.thread_tracker.is_some());
        assert!(config.cpu_util.is_some());

        let cpu_config = config.cpu_util.as_ref().unwrap();
        assert_eq!(cpu_config.frequency, 2000);
        assert_eq!(cpu_config.pid_filters, vec![1234, 5678]);
    }

    #[test]
    fn test_default_config() {
        let config_str = r#"
thread_tracker = {}
cpu_util = {}
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        assert!(config.thread_tracker.is_some());
        assert!(config.cpu_util.is_some());

        let cpu_config = config.cpu_util.as_ref().unwrap();
        assert_eq!(cpu_config.frequency, 9);
        assert!(cpu_config.pid_filters.is_empty());
    }

    #[test]
    fn test_empty_config() {
        let config_str = r#""#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        assert!(config.thread_tracker.is_none());
        assert!(config.cpu_util.is_none());
    }

    #[test]
    fn test_process_filtering_config() {
        let config_str = r#"
filter_process = ["global_filter"]

[thread_tracker]

[cpu_util]
filter_process = ["chrome", "firefox"]

[profiler]
filter_process = ["node"]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        assert!(config.thread_tracker.is_some());
        assert!(config.cpu_util.is_some());
        assert!(config.profiler.is_some());

        // Test global filter
        assert_eq!(config.filter_process, vec!["global_filter"]);

        // Test individual filters before build
        let cpu_config = config.cpu_util.as_ref().unwrap();
        assert_eq!(cpu_config.filter_process, vec!["chrome", "firefox"]);

        let profiler_config = config.profiler.as_ref().unwrap();
        assert_eq!(profiler_config.filter_process, vec!["node"]);
    }

    #[test]
    fn test_process_filtering_requires_threadtrack() {
        let config_str = r#"
[cpu_util]
filter_process = ["test"]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        let result = config.build();

        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("Thread tracker must be enabled"));
        }
    }

    #[test]
    fn test_perfcounter_config_parsing() {
        let config_str = r#"
[perfcounter]
frequency = 100
counters = ["cpu-cycles", "cache-misses", "instructions"]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        assert!(config.perfcounter.is_some());

        let pc_config = config.perfcounter.as_ref().unwrap();
        assert_eq!(pc_config.frequency, 100);
        assert_eq!(pc_config.counters.len(), 3);

        match &pc_config.counters[0] {
            crate::perfcounter::CounterConfig::Named(name) => assert_eq!(name, "cpu-cycles"),
            _ => panic!("Expected Named variant"),
        }
    }

    #[test]
    fn test_perfcounter_mixed_counters() {
        let config_str = r#"
[perfcounter]
frequency = 100
counters = [
    "cpu-cycles",
    "cache-misses",
    { name = "my-raw-event", type = 4, config = 192 }
]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        let pc_config = config.perfcounter.as_ref().unwrap();
        assert_eq!(pc_config.counters.len(), 3);

        match &pc_config.counters[0] {
            crate::perfcounter::CounterConfig::Named(name) => assert_eq!(name, "cpu-cycles"),
            _ => panic!("Expected Named variant"),
        }

        match &pc_config.counters[2] {
            crate::perfcounter::CounterConfig::Custom {
                name,
                perf_type,
                config,
            } => {
                assert_eq!(name, "my-raw-event");
                assert_eq!(*perf_type, 4);
                assert_eq!(*config, 192);
            }
            _ => panic!("Expected Custom variant"),
        }
    }

    #[test]
    fn test_perfcounter_ipc_expansion() {
        let config_str = r#"
[perfcounter]
counters = ["ipc", "cache-misses"]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        let mut pc_config = config.perfcounter.unwrap();

        assert_eq!(pc_config.counters.len(), 2);
        let (derived_info, _is_derived_only) = pc_config.expand_counters().unwrap();
        assert_eq!(pc_config.counters.len(), 3);
        assert_eq!(derived_info.len(), 1);

        match &pc_config.counters[0] {
            crate::perfcounter::CounterConfig::Named(name) => assert_eq!(name, "cpu-cycles"),
            _ => panic!("Expected Named variant"),
        }
        match &pc_config.counters[1] {
            crate::perfcounter::CounterConfig::Named(name) => assert_eq!(name, "cpu-instructions"),
            _ => panic!("Expected Named variant"),
        }
        match &pc_config.counters[2] {
            crate::perfcounter::CounterConfig::Named(name) => assert_eq!(name, "cache-misses"),
            _ => panic!("Expected Named variant"),
        }

        assert_eq!(derived_info[0].0, "ipc");
    }

    #[test]
    fn test_perfcounter_all_counter_types() {
        let config_str = r#"
[perfcounter]
counters = [
    "cpu-cycles",
    "instructions",
    "cpu-instructions",
    "branches",
    "branch-instructions",
    "faults",
    "page-faults",
    "cs",
    "context-switches",
    "migrations",
    "cpu-migrations"
]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        let pc_config = config.perfcounter.as_ref().unwrap();
        assert_eq!(pc_config.counters.len(), 11);

        for counter in &pc_config.counters {
            match counter {
                crate::perfcounter::CounterConfig::Named(_) => {}
                _ => panic!("Expected all to be Named variants"),
            }
        }
    }
}
