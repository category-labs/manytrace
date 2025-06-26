use bpf::BpfConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub global: GlobalConfig,

    #[serde(default)]
    pub bpf: BpfConfig,

    #[serde(default)]
    pub user: Vec<UserConfig>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default = "default_buffer_size")]
    pub buffer_size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserConfig {
    pub socket: String,
    #[serde(default = "default_log_filter")]
    pub log_filter: String,
    #[serde(default)]
    pub random_process_id: Option<bool>,
}

fn default_log_filter() -> String {
    "DEBUG".to_string()
}

fn default_buffer_size() -> usize {
    8 << 20
}

impl Config {
    pub fn load(path: &str) -> eyre::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
