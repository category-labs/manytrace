use std::sync::Arc;
use thiserror::Error;

use crate::{AgentError, Producer};

#[derive(Error, Debug)]
pub enum ExtensionError {
    #[error("validation error: {0}")]
    ValidationError(String),
}

pub struct AgentHandle {
    producer: Arc<Producer>,
}

impl AgentHandle {
    pub(crate) fn new(producer: Arc<Producer>) -> Self {
        Self { producer }
    }

    pub fn submit(&self, event: &protocol::Event) -> Result<(), AgentError> {
        self.producer.submit(event)
    }
}

pub trait Extension: Send + Sync + 'static {
    type Args;
    fn start(&self, args: &Self::Args, handle: &AgentHandle) -> Result<(), ExtensionError>;

    fn stop(&self) -> Result<(), ExtensionError>;
}
