use mpscbuf::Producer;
use parking_lot::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tracing::{debug, warn};

use crate::thread::socket_listener_thread;
use crate::{AgentError, Result};

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

    pub fn submit(&self, data: &[u8]) -> Result<()> {
        let producer = self.producer.lock();
        match producer.as_ref() {
            Some(producer) => match producer.reserve(data.len()) {
                Ok(mut reserved) => {
                    reserved.copy_from_slice(data);
                    drop(reserved);
                    debug!(size = data.len(), "submitted trace event");
                    Ok(())
                }
                Err(e) => {
                    warn!(error = ?e, "failed to reserve space in ring buffer");
                    Err(AgentError::Mpscbuf(e))
                }
            },
            None => Err(AgentError::NotEnabled),
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
    use mpscbuf::Consumer;
    use nix::sys::socket::{recv, sendmsg, ControlMessage, MsgFlags};
    use protocol::{ControlMessage as ProtocolControlMessage, LogLevel};
    use rstest::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
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
    fn consumer_with_fds() -> (Consumer, std::os::fd::OwnedFd, std::os::fd::OwnedFd, usize) {
        let buffer_size = 1024 * 1024;
        let consumer = Consumer::new(buffer_size).unwrap();
        let memory_fd = consumer.memory_fd().try_clone_to_owned().unwrap();
        let notification_fd = consumer.notification_fd().try_clone_to_owned().unwrap();
        (consumer, memory_fd, notification_fd, buffer_size)
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
    fn test_successful_start_sends_ack(
        temp_socket_path: String,
        consumer_with_fds: (Consumer, std::os::fd::OwnedFd, std::os::fd::OwnedFd, usize),
    ) {
        init_tracing();
        let (_consumer, memory_fd, notification_fd, buffer_size) = consumer_with_fds;
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let stream = UnixStream::connect(&temp_socket_path).unwrap();

        let start_msg = ProtocolControlMessage::Start {
            buffer_size: buffer_size as u64,
            log_level: LogLevel::Debug,
        };
        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();

        let size_iov = [std::io::IoSlice::new(&buf)];
        let fds = [memory_fd.as_raw_fd(), notification_fd.as_raw_fd()];
        let cmsg = ControlMessage::ScmRights(&fds);

        sendmsg::<()>(
            stream.as_raw_fd(),
            &size_iov,
            &[cmsg],
            MsgFlags::empty(),
            None,
        )
        .unwrap();

        let mut response_buf = [0u8; 1024];
        let bytes_read = recv(stream.as_raw_fd(), &mut response_buf, MsgFlags::empty()).unwrap();

        let archived_response =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(
                &response_buf[..bytes_read],
            )
            .unwrap();

        match &*archived_response {
            protocol::ArchivedControlMessage::Ack => {}
            _ => panic!("Expected Ack response"),
        }
    }

    #[rstest]
    fn test_duplicate_start_sends_nack(
        temp_socket_path: String,
        consumer_with_fds: (Consumer, std::os::fd::OwnedFd, std::os::fd::OwnedFd, usize),
    ) {
        init_tracing();
        let (_consumer, memory_fd, notification_fd, buffer_size) = consumer_with_fds;
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let stream1 = UnixStream::connect(&temp_socket_path).unwrap();

        let start_msg = ProtocolControlMessage::Start {
            buffer_size: buffer_size as u64,
            log_level: LogLevel::Debug,
        };
        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();

        let size_iov = [std::io::IoSlice::new(&buf)];
        let fds = [memory_fd.as_raw_fd(), notification_fd.as_raw_fd()];
        let cmsg = ControlMessage::ScmRights(&fds);

        sendmsg::<()>(
            stream1.as_raw_fd(),
            &size_iov,
            &[cmsg],
            MsgFlags::empty(),
            None,
        )
        .unwrap();

        let mut response_buf = [0u8; 1024];
        let _bytes_read = recv(stream1.as_raw_fd(), &mut response_buf, MsgFlags::empty()).unwrap();

        let stream2 = UnixStream::connect(&temp_socket_path).unwrap();

        let size_iov2 = [std::io::IoSlice::new(&buf)];
        sendmsg::<()>(
            stream2.as_raw_fd(),
            &size_iov2,
            &[],
            MsgFlags::empty(),
            None,
        )
        .unwrap();

        let mut response_buf2 = [0u8; 1024];
        let bytes_read2 = recv(stream2.as_raw_fd(), &mut response_buf2, MsgFlags::empty()).unwrap();

        let archived_response2 = rkyv::access::<
            protocol::ArchivedControlMessage,
            rkyv::rancor::Error,
        >(&response_buf2[..bytes_read2])
        .unwrap();

        match &*archived_response2 {
            protocol::ArchivedControlMessage::Nack { error } => {
                assert!(error
                    .as_bytes()
                    .windows(b"producer already exists".len())
                    .any(|w| w == b"producer already exists"));
            }
            _ => panic!("Expected Nack response"),
        }
    }

    #[rstest]
    fn test_non_start_first_message_drops_connection(temp_socket_path: String) {
        init_tracing();
        let _agent = Agent::new(temp_socket_path.clone()).unwrap();

        wait_for_socket(&temp_socket_path);

        let stream = UnixStream::connect(&temp_socket_path).unwrap();

        let continue_msg = ProtocolControlMessage::Continue;
        let serialized_len = protocol::compute_length(&continue_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&continue_msg, &mut buf).unwrap();

        let iov = [std::io::IoSlice::new(&buf)];
        sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).unwrap();

        let mut response_buf = [0u8; 1024];
        let bytes_read = recv(stream.as_raw_fd(), &mut response_buf, MsgFlags::empty()).unwrap();
        assert_eq!(bytes_read, 0);
    }
}
