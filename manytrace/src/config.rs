// Copyright (C) 2025 Category Labs, Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

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
