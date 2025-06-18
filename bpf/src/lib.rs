use blazesym::symbolize::Symbolizer;
use protocol::Event;
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;
use tracing::debug;

pub mod cpuutil;
mod perf_event;
pub mod profiler;
pub mod threadtrack;

pub use cpuutil::CpuUtilConfig;
pub use profiler::ProfilerConfig;
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
        let threadtrack = if let Some(_cfg) = self.thread_tracker {
            debug!("initializing thread tracker");
            Some(threadtrack::Object::new())
        } else {
            None
        };

        let cpuutils = if let Some(cfg) = self.cpu_util {
            debug!(
                module = "cpuutil",
                interval_ms = cfg.interval_ms,
                pid_filters = ?cfg.pid_filters,
                "initializing cpu utilization monitor"
            );
            Some(cpuutil::Object::new(cfg))
        } else {
            None
        };

        let profiler = if let Some(cfg) = self.profiler {
            debug!(
                module = "profiler",
                sample_freq = cfg.sample_freq,
                kernel_samples = cfg.kernel_samples,
                user_samples = cfg.user_samples,
                pid_filters = ?cfg.pid_filters,
                "initializing profiler"
            );
            Some(profiler::Object::new(cfg))
        } else {
            None
        };

        Ok(BpfObject {
            symbolizer: Symbolizer::new(),
            threadtrack,
            cpuutils,
            profiler,
        })
    }
}

pub struct BpfObject {
    symbolizer: Symbolizer,
    threadtrack: Option<threadtrack::Object>,
    cpuutils: Option<cpuutil::Object>,
    profiler: Option<profiler::Object>,
}

impl BpfObject {
    pub fn consumer<'this, F>(
        &'this mut self,
        callback: F,
    ) -> Result<BpfConsumer<'this, F>, BpfError>
    where
        F: for<'a> FnMut(Event<'a>) + Clone + 'this,
    {
        let threadtrack = if let Some(ref mut obj) = self.threadtrack {
            Some(obj.build(callback.clone())?)
        } else {
            None
        };

        let cpuutil = if let Some(ref mut obj) = self.cpuutils {
            Some(obj.build(callback.clone())?)
        } else {
            None
        };

        let profiler = if let Some(ref mut obj) = self.profiler {
            Some(obj.build(callback.clone(), &self.symbolizer)?)
        } else {
            None
        };

        Ok(BpfConsumer {
            threadtrack,
            cpuutil,
            profiler,
        })
    }
}

pub struct BpfConsumer<'this, F> {
    threadtrack: Option<threadtrack::ThreadTracker<'this, F>>,
    cpuutil: Option<cpuutil::CpuUtil<'this, F>>,
    profiler: Option<profiler::Profiler<'this, F>>,
}

impl<'this, F> BpfConsumer<'this, F>
where
    F: for<'a> FnMut(Event<'a>) + 'this,
{
    pub fn consume(&mut self) -> Result<(), BpfError> {
        if let Some(ref mut tracker) = self.threadtrack {
            tracker.consume()?;
        }
        if let Some(ref mut util) = self.cpuutil {
            util.consume()?;
        }
        if let Some(ref mut prof) = self.profiler {
            prof.consume()?;
        }
        Ok(())
    }

    pub fn poll(&mut self, timeout: std::time::Duration) -> Result<(), BpfError> {
        if let Some(ref mut tracker) = self.threadtrack {
            tracker.poll(timeout)?;
        }
        if let Some(ref mut util) = self.cpuutil {
            util.poll(timeout)?;
        }
        if let Some(ref mut prof) = self.profiler {
            prof.poll(timeout)?;
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
interval_ms = 2000
pid_filters = [1234, 5678]
"#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        assert!(config.thread_tracker.is_some());
        assert!(config.cpu_util.is_some());

        let cpu_config = config.cpu_util.as_ref().unwrap();
        assert_eq!(cpu_config.interval_ms, 2000);
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
        assert_eq!(cpu_config.interval_ms, 1000);
        assert!(cpu_config.pid_filters.is_empty());
    }

    #[test]
    fn test_empty_config() {
        let config_str = r#""#;

        let config = BpfConfig::from_toml_str(config_str).unwrap();
        assert!(config.thread_tracker.is_none());
        assert!(config.cpu_util.is_none());
    }
}
