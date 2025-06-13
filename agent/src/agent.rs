use parking_lot::Mutex;
use protocol::Event;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::thread::socket_listener_thread;
use crate::{Producer, Result};

pub struct Agent {
    producer: Arc<Mutex<Option<Producer>>>,
    #[allow(dead_code)]
    last_continue_ns: Arc<AtomicU64>,
    _socket_thread: JoinHandle<Result<()>>,
    socket_path: String,
}

impl Agent {
    pub fn new(socket_path: String) -> Result<Self> {
        let producer = Arc::new(Mutex::new(None));
        let last_continue_ns = Arc::new(AtomicU64::new(0));
        let producer_clone = producer.clone();
        let last_continue_clone = last_continue_ns.clone();
        let socket_path_clone = socket_path.clone();

        let socket_thread = thread::Builder::new()
            .name("agent-listener".to_string())
            .spawn(move || {
                socket_listener_thread(socket_path_clone, producer_clone, last_continue_clone)
            })?;

        Ok(Agent {
            producer,
            last_continue_ns,
            _socket_thread: socket_thread,
            socket_path,
        })
    }

    pub fn enabled(&self) -> bool {
        self.producer.lock().is_some()
    }

    pub fn submit(&self, event: &Event) -> Result<()> {
        let producer = self.producer.lock();
        match producer.as_ref() {
            Some(producer) => producer.submit(event),
            None => Err(crate::AgentError::NotEnabled),
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentClient, Consumer};
    use protocol::{Counter, Event, Labels, LogLevel};
    use rstest::*;
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
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
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
            labels: Labels::new(),
        });

        agent.submit(&event).unwrap();
    }
}
