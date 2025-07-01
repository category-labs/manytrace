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
