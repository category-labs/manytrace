use agent::{AgentHandle, Extension, ExtensionError};
use arc_swap::ArcSwap;
use protocol::{ArchivedTracingArgs, Event};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

struct TracingState {
    clock_id: libc::clockid_t,
    env_filter: Option<Arc<EnvFilter>>,
    handle: Option<AgentHandle>,
}

pub struct TracingExtension {
    state: Arc<ArcSwap<TracingState>>,
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
            state: Arc::new(ArcSwap::from_pointee(TracingState {
                clock_id: libc::CLOCK_MONOTONIC,
                env_filter: None,
                handle: None,
            })),
        }
    }

    pub fn clock_id(&self) -> libc::clockid_t {
        self.state.load().clock_id
    }

    pub fn with_env_filter<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&EnvFilter) -> R,
    {
        let state = self.state.load();
        state
            .env_filter
            .as_ref()
            .map(|arc_filter| f(arc_filter.as_ref()))
    }

    pub fn submit(&self, event: &Event) -> Result<(), agent::AgentError> {
        let state = self.state.load();
        match &state.handle {
            Some(handle) => handle.submit(event),
            None => Err(agent::AgentError::NotEnabled),
        }
    }

    pub fn is_active(&self) -> bool {
        self.state.load().handle.is_some()
    }
}

impl Extension for TracingExtension {
    type Args = ArchivedTracingArgs;

    fn start(
        &self,
        args: &ArchivedTracingArgs,
        handle: &AgentHandle,
    ) -> Result<(), ExtensionError> {
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
            env_filter: Some(Arc::new(env_filter)),
            handle: Some(handle.clone()),
        });

        self.state.store(new_state);

        tracing::debug!(
            "tracing extension started with clock_id: {}, log_filter: {}",
            clock_id,
            log_filter
        );

        Ok(())
    }

    fn stop(&self) -> Result<(), ExtensionError> {
        let current_state = self.state.load_full();
        let new_state = Arc::new(TracingState {
            clock_id: current_state.clock_id,
            env_filter: current_state.env_filter.clone(),
            handle: None,
        });
        self.state.store(new_state);
        Ok(())
    }
}
