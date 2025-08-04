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

use agent::{AgentHandle, Extension, ExtensionError};
use arc_swap::ArcSwapOption;
use protocol::{ArchivedTracingArgs, Event};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

struct TracingState {
    clock_id: libc::clockid_t,
    env_filter: EnvFilter,
    handle: AgentHandle,
}

pub struct TracingExtension {
    state: Arc<ArcSwapOption<TracingState>>,
}

impl Clone for TracingExtension {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl Default for TracingExtension {
    fn default() -> Self {
        Self::new()
    }
}

impl TracingExtension {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ArcSwapOption::from(None)),
        }
    }

    pub fn clock_id(&self) -> libc::clockid_t {
        self.state
            .load()
            .as_ref()
            .as_ref()
            .map(|s| s.clock_id)
            .unwrap_or(libc::CLOCK_MONOTONIC)
    }

    pub fn with_env_filter<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&EnvFilter) -> R,
    {
        self.state
            .load()
            .as_ref()
            .as_ref()
            .map(|s| f(&s.env_filter))
    }

    pub fn submit(&self, event: &Event) -> Result<(), agent::AgentError> {
        match self.state.load().as_ref() {
            Some(state) => state.handle.submit(event),
            None => Err(agent::AgentError::NotEnabled),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state.load().is_some()
    }
}

impl Extension for TracingExtension {
    type Args = ArchivedTracingArgs;

    fn start(&self, args: &ArchivedTracingArgs, handle: AgentHandle) -> Result<(), ExtensionError> {
        use protocol::ArchivedTimestampType;

        let clock_id = match args.timestamp_type {
            ArchivedTimestampType::Monotonic => libc::CLOCK_MONOTONIC,
            ArchivedTimestampType::Boottime => libc::CLOCK_BOOTTIME,
            ArchivedTimestampType::Realtime => libc::CLOCK_REALTIME,
        };

        let log_filter = args.log_filter.as_str();
        let env_filter = EnvFilter::try_new(log_filter).map_err(|e| {
            ExtensionError::ValidationError(format!("invalid log filter '{}': {}", log_filter, e))
        })?;

        let new_state = Arc::new(TracingState {
            clock_id,
            env_filter,
            handle,
        });

        self.state.store(Some(new_state));

        tracing::debug!(
            "tracing extension started with clock_id: {}, log_filter: {}",
            clock_id,
            log_filter
        );

        Ok(())
    }

    fn stop(&self) -> Result<(), ExtensionError> {
        self.state.store(None);
        Ok(())
    }
}
