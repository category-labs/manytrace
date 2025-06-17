use protocol::Event;
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

pub mod cpuutil;
mod perf_event;
pub mod threadtrack;

pub use cpuutil::CpuUtilConfig;
pub use threadtrack::ThreadTrackerConfig;

#[derive(Error, Debug)]
pub enum BpfError {
    #[error("Failed to load BPF program: {0}")]
    LoadError(String),
    #[error("Failed to attach BPF program: {0}")]
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
}

impl BpfConfig {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, BpfError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| BpfError::LoadError(format!("Failed to read config file: {}", e)))?;

        toml::from_str(&content)
            .map_err(|e| BpfError::LoadError(format!("Failed to parse TOML: {}", e)))
    }

    pub fn from_toml_str(content: &str) -> Result<Self, BpfError> {
        toml::from_str(content)
            .map_err(|e| BpfError::LoadError(format!("Failed to parse TOML: {}", e)))
    }

    pub fn build(self) -> Result<BpfObject, BpfError> {
        let threadtrack = self.thread_tracker.map(|_| threadtrack::Object::new());
        let cpuutils = self.cpu_util.map(cpuutil::Object::new);

        Ok(BpfObject {
            threadtrack,
            cpuutils,
        })
    }
}

pub struct BpfObject {
    threadtrack: Option<threadtrack::Object>,
    cpuutils: Option<cpuutil::Object>,
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

        Ok(BpfConsumer {
            threadtrack,
            cpuutil,
        })
    }
}

pub struct BpfConsumer<'this, F> {
    threadtrack: Option<threadtrack::ThreadTracker<'this, F>>,
    cpuutil: Option<cpuutil::CpuUtil<'this, F>>,
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
        Ok(())
    }

    pub fn poll(&mut self, timeout: std::time::Duration) -> Result<(), BpfError> {
        if let Some(ref mut tracker) = self.threadtrack {
            tracker.poll(timeout)?;
        }
        if let Some(ref mut util) = self.cpuutil {
            util.poll(timeout)?;
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
