use arc_swap::ArcSwapOption;
use protocol::Event;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use thread_local::ThreadLocal;

use crate::epoll_thread::epoll_listener_thread;
use crate::{Producer, Result};

/// Producer state containing producer and thread tracking.
pub(crate) struct ProducerState {
    producer: Arc<Producer>,
    clock_id: libc::clockid_t,
    thread_names_sent: Arc<ThreadLocal<std::cell::Cell<bool>>>,
}

impl ProducerState {
    pub(crate) fn new(producer: Arc<Producer>, clock_id: libc::clockid_t) -> Self {
        Self {
            producer,
            clock_id,
            thread_names_sent: Arc::new(ThreadLocal::new()),
        }
    }

    pub(crate) fn clock_id(&self) -> libc::clockid_t {
        self.clock_id
    }

    pub(crate) fn submit_with_thread_name(&self, event: &Event) -> Result<()> {
        let thread_sent = self
            .thread_names_sent
            .get_or(|| std::cell::Cell::new(false));

        if !thread_sent.get() {
            if let Some(thread_name) = std::thread::current().name() {
                let thread_event = protocol::Event::ThreadName(protocol::ThreadName {
                    name: thread_name,
                    tid: unsafe { libc::syscall(libc::SYS_gettid) } as i32,
                    pid: std::process::id() as i32,
                });

                if let Err(e) = self.producer.submit(&thread_event) {
                    tracing::debug!(error = ?e, "failed to submit thread name event");
                }
            }
            thread_sent.set(true);
        }

        self.producer.submit(event)
    }
}

/// Server that listens for client connections and receives events.
pub struct Agent {
    producer_state: Arc<ArcSwapOption<ProducerState>>,
    socket_thread: Option<JoinHandle<Result<()>>>,
    socket_path: String,
    shutdown: Arc<AtomicBool>,
}

impl Agent {
    /// Create a new agent listening on the given socket path.
    pub fn new(socket_path: String) -> Result<Self> {
        Self::with_timeout(
            socket_path,
            (crate::agent_state::CLIENT_TIMEOUT_NS / 1_000_000) as u16,
        )
    }

    /// Create a new agent with custom client keepalive timeout.
    pub fn with_timeout(socket_path: String, timeout_ms: u16) -> Result<Self> {
        let producer_state = Arc::new(ArcSwapOption::empty());
        let producer_state_clone = producer_state.clone();
        let socket_path_clone = socket_path.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let socket_thread = thread::Builder::new()
            .name("manytrace-agent".to_string())
            .spawn(move || {
                epoll_listener_thread(
                    socket_path_clone,
                    producer_state_clone,
                    shutdown_clone,
                    timeout_ms,
                )
            })?;

        Ok(Agent {
            producer_state,
            socket_thread: Some(socket_thread),
            socket_path,
            shutdown,
        })
    }

    /// Check if a client is currently connected.
    pub fn enabled(&self) -> bool {
        self.producer_state.load().is_some()
    }

    /// Submit an event to connected clients.
    pub fn submit(&self, event: &Event) -> Result<()> {
        let producer_state = self.producer_state.load();
        match producer_state.as_ref() {
            Some(state) => state.submit_with_thread_name(event),
            None => Err(crate::AgentError::NotEnabled),
        }
    }

    /// Get the current clock ID if a client is connected.
    pub fn clock_id(&self) -> libc::clockid_t {
        self.producer_state
            .load()
            .as_ref()
            .map(|s| s.clock_id())
            .unwrap_or(libc::CLOCK_MONOTONIC)
    }

    /// Gracefully shut down the agent and wait for thread termination.
    pub fn wait_terminated(&mut self) -> Result<()> {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.socket_thread.take() {
            thread
                .join()
                .map_err(|_| crate::AgentError::Io(std::io::Error::other("Thread panicked")))??;
        }
        Ok(())
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentClient, Consumer};
    use protocol::{Counter, Event, Labels, LogLevel};
    use rstest::*;
    use std::borrow::Cow;
    use std::sync::Once;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    static INIT: Once = Once::new();

    fn init_tracing() {
        INIT.call_once(|| {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
                )
                .init();
        });
    }

    #[fixture]
    fn temp_socket_path() -> String {
        let dir = tempdir().unwrap();
        let socket_path = dir
            .path()
            .join("agent_test.sock")
            .to_string_lossy()
            .to_string();
        std::mem::forget(dir);
        socket_path
    }

    fn wait_for_socket(socket_path: &str) {
        for _ in 0..50 {
            if std::path::Path::new(socket_path).exists() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "Socket did not appear within timeout at path: {}",
            socket_path
        );
    }

    #[fixture]
    fn consumer() -> Consumer {
        let buffer_size = 1024 * 1024;
        Consumer::new(buffer_size).unwrap()
    }

    #[rstest]
    fn test_agent_creation(temp_socket_path: String) {
        init_tracing();
        let agent = Agent::new(temp_socket_path.clone()).unwrap();
        assert!(!agent.enabled());

        wait_for_socket(&temp_socket_path);

        assert!(std::path::Path::new(&temp_socket_path).exists());
    }

    #[rstest]
    fn test_successful_start_sends_ack(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, LogLevel::Debug).unwrap();
        assert!(client.enabled());
    }

    #[rstest]
    fn test_duplicate_start_sends_nack(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client1 = AgentClient::new(temp_socket_path.clone());
        client1.start(&consumer, LogLevel::Debug).unwrap();
        assert!(client1.enabled());

        let mut client2 = AgentClient::new(temp_socket_path);
        let result = client2.start(&consumer, LogLevel::Debug);
        assert!(result.is_err());
        assert!(!client2.enabled());
    }

    #[rstest]
    fn test_client_send_continue(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, LogLevel::Debug).unwrap();
        assert!(client.enabled());

        client.send_continue().unwrap();
        client.stop().unwrap();
        assert!(!client.enabled());
    }

    #[rstest]
    fn test_agent_submit_event(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, LogLevel::Debug).unwrap();

        thread::sleep(Duration::from_millis(10));

        let event = Event::Counter(Counter {
            name: "test_metric",
            value: 123.456,
            timestamp: 1234567890,
            tid: 1,
            pid: 2,
            labels: Cow::Owned(Labels::new()),
        });

        agent.submit(&event).unwrap();
    }

    #[rstest]
    fn test_keepalive_timeout(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let timeout_ms = 100;
        let agent = Agent::with_timeout(temp_socket_path.clone(), timeout_ms).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, LogLevel::Debug).unwrap();

        thread::sleep(Duration::from_millis(10));

        let event = Event::Counter(Counter {
            name: "test_metric",
            value: 123.456,
            timestamp: 1234567890,
            tid: 1,
            pid: 2,
            labels: Cow::Owned(Labels::new()),
        });

        agent.submit(&event).unwrap();

        thread::sleep(Duration::from_millis(timeout_ms as u64 * 2));

        let result = agent.submit(&event);
        assert!(result.is_err());
    }

    #[rstest]
    fn test_wait_terminated(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let mut agent = Agent::with_timeout(temp_socket_path.clone(), 100).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path.clone());
        client.start(&consumer, LogLevel::Debug).unwrap();

        thread::sleep(Duration::from_millis(10));

        agent.wait_terminated().unwrap();
    }
}
