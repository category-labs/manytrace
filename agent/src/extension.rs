use std::sync::Arc;
use thiserror::Error;
use thread_local::ThreadLocal;

use crate::{get_process_id, AgentError, Producer};

#[derive(Error, Debug)]
pub enum ExtensionError {
    #[error("validation error: {0}")]
    ValidationError(String),
}

#[derive(Clone)]
pub struct AgentHandle {
    producer: Arc<Producer>,
    thread_names_sent: Arc<ThreadLocal<std::cell::Cell<bool>>>,
}

impl AgentHandle {
    pub(crate) fn new(producer: Arc<Producer>) -> Self {
        Self {
            producer,
            thread_names_sent: Arc::new(ThreadLocal::new()),
        }
    }

    pub fn submit(&self, event: &protocol::Event) -> Result<(), AgentError> {
        let thread_sent = self
            .thread_names_sent
            .get_or(|| std::cell::Cell::new(false));

        if !thread_sent.get() {
            if let Some(thread_name) = std::thread::current().name() {
                let thread_event = protocol::Event::ThreadName(protocol::ThreadName {
                    name: thread_name,
                    tid: unsafe { libc::syscall(libc::SYS_gettid) } as i32,
                    pid: get_process_id(),
                });

                self.producer.submit(&thread_event)?;
            }
            thread_sent.set(true);
        }

        self.producer.submit(event)
    }
}

pub trait Extension: Send + Sync + 'static {
    type Args;
    fn start(&self, args: &Self::Args, handle: AgentHandle) -> Result<(), ExtensionError>;

    fn stop(&self) -> Result<(), ExtensionError>;
}
