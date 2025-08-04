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
                let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
                let pid = get_process_id();
                let thread_event = protocol::Event::Track(protocol::Track {
                    name: thread_name,
                    track_type: protocol::TrackType::Thread { tid, pid },
                    parent: Some(protocol::TrackType::Process { pid }),
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
