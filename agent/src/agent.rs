use protocol::ArchivedTracingArgs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::epoll_thread::epoll_listener_thread;
use crate::extension::Extension;
use crate::Result;

pub struct AgentBuilder {
    socket_path: String,
    timeout_ms: u16,
    tracing_extension: Option<Box<dyn Extension<Args = ArchivedTracingArgs>>>,
}

impl AgentBuilder {
    pub fn new(socket_path: String) -> Self {
        Self {
            socket_path,
            timeout_ms: (crate::agent_state::CLIENT_TIMEOUT_NS / 1_000_000) as u16,
            tracing_extension: None,
        }
    }

    pub fn with_timeout(mut self, timeout_ms: u16) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn register_tracing(mut self, ext: Box<dyn Extension<Args = ArchivedTracingArgs>>) -> Self {
        self.tracing_extension = Some(ext);
        self
    }

    pub fn build(self) -> Result<Agent> {
        Agent::internal_new(self.socket_path, self.timeout_ms, self.tracing_extension)
    }
}

pub struct Agent {
    socket_thread: Option<JoinHandle<Result<()>>>,
    socket_path: String,
    shutdown: Arc<AtomicBool>,
}

impl Agent {
    pub fn new(socket_path: String) -> Result<Self> {
        Self::with_timeout(
            socket_path,
            (crate::agent_state::CLIENT_TIMEOUT_NS / 1_000_000) as u16,
        )
    }

    pub fn with_timeout(socket_path: String, timeout_ms: u16) -> Result<Self> {
        Self::internal_new(socket_path, timeout_ms, None)
    }

    fn internal_new(
        socket_path: String,
        timeout_ms: u16,
        tracing_extension: Option<Box<dyn Extension<Args = ArchivedTracingArgs>>>,
    ) -> Result<Self> {
        let socket_path_clone = socket_path.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let agent_state = crate::agent_state::AgentState::new(timeout_ms, tracing_extension);

        let socket_thread = thread::Builder::new()
            .name("manytrace-agent".to_string())
            .spawn(move || epoll_listener_thread(socket_path_clone, shutdown_clone, agent_state))?;

        Ok(Agent {
            socket_thread: Some(socket_thread),
            socket_path,
            shutdown,
        })
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
    use protocol::{Event, Labels};
    use rstest::*;
    use std::borrow::Cow;
    use std::sync::Once;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    static INIT: Once = Once::new();

    fn init_tracing() {
        INIT.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error")),
                )
                .try_init();
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
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        assert!(std::path::Path::new(&temp_socket_path).exists());
    }

    #[rstest]
    fn test_successful_start_sends_ack(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, "debug".to_string()).unwrap();
        assert!(client.enabled());
    }

    #[rstest]
    fn test_duplicate_start_sends_nack(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client1 = AgentClient::new(temp_socket_path.clone());
        client1.start(&consumer, "debug".to_string()).unwrap();
        assert!(client1.enabled());

        let mut client2 = AgentClient::new(temp_socket_path);
        let result = client2.start(&consumer, "debug".to_string());
        assert!(result.is_err());
        assert!(!client2.enabled());
    }

    #[rstest]
    fn test_client_send_continue(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, "debug".to_string()).unwrap();
        assert!(client.enabled());

        client.send_continue().unwrap();
        client.stop().unwrap();
        assert!(!client.enabled());
    }

    #[rstest]
    fn test_agent_submit_event(temp_socket_path: String, consumer: Consumer) {
        init_tracing();

        let (ext, _started, _stopped, events) = TestExtension::new(false);
        let events_clone = events.clone();

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let event_name = "test_event_submitted";
            events_clone.lock().unwrap().push(event_name.to_string());
        });

        let _agent = AgentBuilder::new(temp_socket_path.clone())
            .register_tracing(Box::new(ext))
            .build()
            .unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, "debug".to_string()).unwrap();

        thread::sleep(Duration::from_millis(100));

        let submitted_events = events.lock().unwrap();
        assert!(submitted_events.contains(&"extension_started".to_string()));
        assert!(submitted_events.contains(&"test_event_submitted".to_string()));
    }

    #[rstest]
    fn test_keepalive_timeout(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let timeout_ms = 100;

        let (ext, started, stopped, _events) = TestExtension::new(false);

        let _agent = AgentBuilder::new(temp_socket_path.clone())
            .with_timeout(timeout_ms)
            .register_tracing(Box::new(ext))
            .build()
            .unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        client.start(&consumer, "debug".to_string()).unwrap();

        thread::sleep(Duration::from_millis(10));

        assert!(*started.lock().unwrap());
        assert!(!*stopped.lock().unwrap());

        thread::sleep(Duration::from_millis(timeout_ms as u64 * 2));

        assert!(*stopped.lock().unwrap());
    }

    #[rstest]
    fn test_wait_terminated(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let mut agent = Agent::with_timeout(temp_socket_path.clone(), 100).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path.clone());
        client.start(&consumer, "debug".to_string()).unwrap();

        thread::sleep(Duration::from_millis(10));

        agent.wait_terminated().unwrap();
    }

    #[rstest]
    fn test_version_rpc_before_start(temp_socket_path: String) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        let version = client.check_version().unwrap();
        assert_eq!(version, protocol::VERSION);
    }

    #[rstest]
    fn test_version_rpc_after_start(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path.clone());
        client.start(&consumer, "debug".to_string()).unwrap();
        assert!(client.enabled());

        let version = client.check_version().unwrap();
        assert_eq!(version, protocol::VERSION);
    }

    #[rstest]
    fn test_version_rpc_multiple_clients(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client1 = AgentClient::new(temp_socket_path.clone());
        client1.start(&consumer, "debug".to_string()).unwrap();

        let mut client2 = AgentClient::new(temp_socket_path.clone());
        let version = client2.check_version().unwrap();
        assert_eq!(version, protocol::VERSION);

        let mut client3 = AgentClient::new(temp_socket_path);
        let version = client3.check_version().unwrap();
        assert_eq!(version, protocol::VERSION);
    }

    #[rstest]
    fn test_version_check_on_start(temp_socket_path: String, consumer: Consumer) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        let result = client.start(&consumer, "debug".to_string());
        assert!(result.is_ok());
        assert!(client.enabled());
    }

    use crate::extension::{AgentHandle, Extension, ExtensionError};
    use protocol::ArchivedTracingArgs;
    use std::sync::Mutex;

    struct TestExtension {
        started: Arc<Mutex<bool>>,
        stopped: Arc<Mutex<bool>>,
        events_submitted: Arc<Mutex<Vec<String>>>,
        fail_on_start: bool,
    }

    type TestExtensionState = (
        TestExtension,
        Arc<Mutex<bool>>,
        Arc<Mutex<bool>>,
        Arc<Mutex<Vec<String>>>,
    );

    impl TestExtension {
        fn new(fail_on_start: bool) -> TestExtensionState {
            let started = Arc::new(Mutex::new(false));
            let stopped = Arc::new(Mutex::new(false));
            let events_submitted = Arc::new(Mutex::new(Vec::new()));

            (
                Self {
                    started: started.clone(),
                    stopped: stopped.clone(),
                    events_submitted: events_submitted.clone(),
                    fail_on_start,
                },
                started,
                stopped,
                events_submitted,
            )
        }
    }

    impl Extension for TestExtension {
        type Args = ArchivedTracingArgs;

        fn start(
            &self,
            _args: &ArchivedTracingArgs,
            handle: AgentHandle,
        ) -> std::result::Result<(), ExtensionError> {
            if self.fail_on_start {
                return Err(ExtensionError::ValidationError(
                    "test error from extension".to_string(),
                ));
            }

            *self.started.lock().unwrap() = true;

            let event = Event::Instant(protocol::Instant {
                name: "extension_started",
                timestamp: 1000,
                track_id: protocol::TrackId::Thread { tid: 1, pid: 1 },
                labels: Cow::Owned(Labels::new()),
            });

            if let Ok(()) = handle.submit(&event) {
                self.events_submitted
                    .lock()
                    .unwrap()
                    .push("extension_started".to_string());
            }

            Ok(())
        }

        fn stop(&self) -> std::result::Result<(), ExtensionError> {
            *self.stopped.lock().unwrap() = true;
            Ok(())
        }
    }

    #[rstest]
    fn test_extension_start_stop(temp_socket_path: String, consumer: Consumer) {
        init_tracing();

        let (ext, started, stopped, _events) = TestExtension::new(false);

        let _agent = AgentBuilder::new(temp_socket_path.clone())
            .register_tracing(Box::new(ext))
            .build()
            .unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path.clone());
        client.start(&consumer, "debug".to_string()).unwrap();

        assert!(*started.lock().unwrap());
        assert!(!*stopped.lock().unwrap());

        client.stop().unwrap();
        drop(client);

        thread::sleep(Duration::from_millis(100));

        assert!(*stopped.lock().unwrap());
    }

    #[rstest]
    fn test_extension_error_propagation(temp_socket_path: String, consumer: Consumer) {
        init_tracing();

        let (ext, started, _stopped, _events) = TestExtension::new(true);

        let _agent = AgentBuilder::new(temp_socket_path.clone())
            .register_tracing(Box::new(ext))
            .build()
            .unwrap();

        wait_for_socket(&temp_socket_path);

        let mut client = AgentClient::new(temp_socket_path);
        let result = client.start(&consumer, "debug".to_string());

        assert!(result.is_err());
        assert!(!client.enabled());

        assert!(!*started.lock().unwrap());
    }
}
