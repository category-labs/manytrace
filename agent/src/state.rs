use mpscbuf::{Producer as MpscProducer, WakeupStrategy};
use nix::sys::socket::{recvmsg, sendmsg, ControlMessageOwned, MsgFlags};
use protocol::ControlMessage;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::{get_timestamp_ns, AgentError, Producer, Result};

pub struct AgentContext {
    pub another_started: bool,
}

pub const CLIENT_TIMEOUT_NS: u64 = 2_000_000_000;

pub enum State {
    Pending,
    Started,
}

pub struct ClientState {
    client_id: u64,
    stream: UnixStream,
    state: State,
    timestamp_ns: u64,
}

impl ClientState {
    pub fn new(client_id: u64, stream: UnixStream) -> Self {
        ClientState {
            client_id,
            stream,
            state: State::Pending,
            timestamp_ns: get_timestamp_ns(),
        }
    }

    pub(crate) fn stream(&self) -> &UnixStream {
        &self.stream
    }

    pub(crate) fn into_stream(self) -> UnixStream {
        self.stream
    }

    pub(crate) fn timer(&self) -> bool {
        let current_time = get_timestamp_ns();
        let elapsed = current_time.saturating_sub(self.timestamp_ns);
        elapsed <= CLIENT_TIMEOUT_NS
    }

    pub(crate) fn handle_message(&mut self, ctx: &mut AgentContext) -> MessageResult {
        match &self.state {
            State::Pending => handle_pending_state(self, ctx),
            State::Started => handle_started_state(self),
        }
    }
}

pub enum MessageResult {
    Continue,
    Started(Arc<Producer>),
    Disconnect,
    Error(AgentError),
}

fn handle_pending_state(client_state: &mut ClientState, ctx: &mut AgentContext) -> MessageResult {
    match handle_start_message(&client_state.stream, ctx) {
        Ok(Some(producer)) => {
            client_state.timestamp_ns = get_timestamp_ns();
            client_state.state = State::Started;
            debug!(client_id = client_state.client_id, "client started");
            MessageResult::Started(producer)
        }
        Ok(None) => MessageResult::Continue,
        Err(e) => {
            warn!(client_id = client_state.client_id, error = ?e, "error handling start message");
            MessageResult::Error(e)
        }
    }
}

fn handle_started_state(client_state: &mut ClientState) -> MessageResult {
    match handle_client_message(&client_state.stream) {
        Ok(Some(protocol::ControlMessage::Stop)) => {
            debug!(client_id = client_state.client_id, "client sent stop");
            MessageResult::Disconnect
        }
        Ok(Some(protocol::ControlMessage::Continue)) => {
            client_state.timestamp_ns = get_timestamp_ns();
            debug!(
                client_id = client_state.client_id,
                timestamp = client_state.timestamp_ns,
                "client sent continue"
            );
            MessageResult::Continue
        }
        Ok(Some(_)) => {
            debug!(
                client_id = client_state.client_id,
                "unexpected message from client"
            );
            MessageResult::Continue
        }
        Ok(None) => {
            debug!(client_id = client_state.client_id, "client disconnected");
            MessageResult::Disconnect
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => MessageResult::Continue,
        Err(e) => {
            warn!(client_id = client_state.client_id, error = ?e, "error handling client message");
            MessageResult::Error(AgentError::Io(e))
        }
    }
}

fn handle_start_message(
    stream: &UnixStream,
    ctx: &mut AgentContext,
) -> Result<Option<Arc<Producer>>> {
    let mut cmsg_buffer = nix::cmsg_space!([RawFd; 2]);
    let mut msg_buf = [0u8; 1024];
    let mut iov = [std::io::IoSliceMut::new(&mut msg_buf)];

    let msg = match recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buffer),
        MsgFlags::empty(),
    ) {
        Ok(msg) => msg,
        Err(nix::errno::Errno::EAGAIN) => return Ok(None),
        Err(e) => return Err(e.into()),
    };

    let received_data = match msg.iovs().next() {
        Some(data) if !data.is_empty() => data,
        _ => return Ok(None),
    };

    let archived_msg =
        rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(received_data)?;

    match archived_msg {
        protocol::ArchivedControlMessage::Start { buffer_size, .. } => {
            if ctx.another_started {
                let error_msg = "another client already started".to_string();
                let nack_msg = ControlMessage::Nack { error: &error_msg };
                send_message(stream, &nack_msg)?;
                debug!("sent nack - another client already started");
                return Ok(None);
            }

            let buffer_size = buffer_size.to_native() as usize;

            let mut memory_fd: Option<OwnedFd> = None;
            let mut notification_fd: Option<OwnedFd> = None;

            for cmsg in msg.cmsgs()? {
                if let ControlMessageOwned::ScmRights(fds) = cmsg {
                    if fds.len() >= 2 {
                        memory_fd = Some(unsafe { OwnedFd::from_raw_fd(fds[0]) });
                        notification_fd = Some(unsafe { OwnedFd::from_raw_fd(fds[1]) });
                        break;
                    }
                }
            }

            let memory_fd = memory_fd.ok_or_else(|| {
                AgentError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to receive memory fd",
                ))
            })?;
            let notification_fd = notification_fd.ok_or_else(|| {
                AgentError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to receive notification fd",
                ))
            })?;

            let new_producer = MpscProducer::new(
                memory_fd,
                notification_fd,
                buffer_size,
                WakeupStrategy::Forced,
            )?;

            let producer = Arc::new(Producer::from_inner(new_producer));

            debug!(buffer_size, "producer initialized");

            let ack_msg = ControlMessage::Ack;
            send_message(stream, &ack_msg)?;
            debug!("sent ack - producer initialized");

            Ok(Some(producer))
        }
        _ => {
            debug!("expected Start message as first message");
            Ok(None)
        }
    }
}

fn handle_client_message(
    stream: &UnixStream,
) -> std::io::Result<Option<protocol::ControlMessage<'static>>> {
    let mut msg_buf = [0u8; 1024];
    let mut iov = [std::io::IoSliceMut::new(&mut msg_buf)];

    let msg = match recvmsg::<()>(stream.as_raw_fd(), &mut iov, None, MsgFlags::empty()) {
        Ok(msg) => msg,
        Err(nix::errno::Errno::EAGAIN) => {
            return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        }
        Err(e) => return Err(e.into()),
    };

    let received_data = match msg.iovs().next() {
        Some(data) if !data.is_empty() => data,
        _ => return Ok(None),
    };

    let archived_msg =
        rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(received_data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    match archived_msg {
        protocol::ArchivedControlMessage::Stop => Ok(Some(protocol::ControlMessage::Stop)),
        protocol::ArchivedControlMessage::Continue => Ok(Some(protocol::ControlMessage::Continue)),
        _ => Ok(None),
    }
}

fn send_message(stream: &UnixStream, msg: &ControlMessage) -> Result<()> {
    let serialized = protocol::compute_length(msg)?;
    let mut buf = vec![0u8; serialized];
    protocol::serialize_to_buf(msg, &mut buf)?;

    let iov = [std::io::IoSlice::new(&buf)];
    sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Consumer;
    use protocol::{ControlMessage, LogLevel};
    use rstest::rstest;
    use std::os::unix::net::{UnixListener, UnixStream};
    use tempfile::TempDir;

    fn create_socket_pair() -> (UnixStream, UnixStream) {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let client = UnixStream::connect(&socket_path).unwrap();
        let (server, _) = listener.accept().unwrap();

        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();

        (client, server)
    }

    fn send_start_message(stream: &UnixStream, buffer_size: u64, log_level: LogLevel) {
        let start_msg = ControlMessage::Start {
            buffer_size,
            log_level,
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();

        use nix::sys::socket::{sendmsg, MsgFlags};
        use std::os::fd::AsRawFd;

        let iov = [std::io::IoSlice::new(&buf)];
        sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).unwrap();
    }

    fn send_start_message_with_fds(stream: &UnixStream, consumer: &Consumer) {
        let start_msg = ControlMessage::Start {
            buffer_size: consumer.data_size() as u64,
            log_level: LogLevel::Debug,
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();

        use nix::sys::socket::{sendmsg, ControlMessage as NixControlMessage, MsgFlags};
        use std::os::fd::AsRawFd;

        let iov = [std::io::IoSlice::new(&buf)];
        let fds = [
            consumer.memory_fd().as_raw_fd(),
            consumer.notification_fd().as_raw_fd(),
        ];
        let cmsg = NixControlMessage::ScmRights(&fds);

        sendmsg::<()>(stream.as_raw_fd(), &iov, &[cmsg], MsgFlags::empty(), None).unwrap();
    }

    fn send_control_message(stream: &UnixStream, msg: ControlMessage) {
        let serialized_len = protocol::compute_length(&msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&msg, &mut buf).unwrap();

        use nix::sys::socket::{sendmsg, MsgFlags};
        use std::os::fd::AsRawFd;

        let iov = [std::io::IoSlice::new(&buf)];
        sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).unwrap();
    }

    #[rstest]
    fn test_client_state_new() {
        let (_client, server) = create_socket_pair();
        let client_id = 42;

        let client_state = ClientState::new(client_id, server);

        assert_eq!(client_state.client_id, 42);
        assert!(matches!(client_state.state, State::Pending));
        assert!(client_state.timestamp_ns > 0);
    }

    #[rstest]
    fn test_client_state_timer_within_timeout() {
        let (_client, server) = create_socket_pair();
        let client_state = ClientState::new(1, server);

        assert!(client_state.timer());
    }

    #[rstest]
    fn test_client_state_timer_after_timeout() {
        let (_client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);

        client_state.timestamp_ns = get_timestamp_ns() - CLIENT_TIMEOUT_NS - 1;

        assert!(!client_state.timer());
    }

    #[rstest]
    fn test_pending_state_no_message() {
        let (_client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);
        let mut ctx = AgentContext {
            another_started: false,
        };

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Continue));
        assert!(matches!(client_state.state, State::Pending));
    }

    #[rstest]
    fn test_pending_state_with_start_no_fds() {
        let (client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);
        let mut ctx = AgentContext {
            another_started: false,
        };

        send_start_message(&client, 1024, LogLevel::Debug);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Error(_)));
    }

    #[rstest]
    fn test_pending_state_successful_start() {
        let (client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);
        let mut ctx = AgentContext {
            another_started: false,
        };

        let consumer = Consumer::new(1024 * 1024).unwrap();
        send_start_message_with_fds(&client, &consumer);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Started(_)));
        assert!(matches!(client_state.state, State::Started));
    }

    #[rstest]
    fn test_pending_state_start_with_existing_producer() {
        let (client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);
        let mut ctx = AgentContext {
            another_started: true,
        };

        let consumer = Consumer::new(1024 * 1024).unwrap();
        send_start_message_with_fds(&client, &consumer);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Continue));
        assert!(matches!(client_state.state, State::Pending));
    }

    #[rstest]
    fn test_started_state_continue_message() {
        let (client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);
        client_state.state = State::Started;
        let old_timestamp = client_state.timestamp_ns;

        let mut ctx = AgentContext {
            another_started: true,
        };

        send_control_message(&client, ControlMessage::Continue);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Continue));
        assert!(matches!(client_state.state, State::Started));
        assert!(client_state.timestamp_ns > old_timestamp);
    }

    #[rstest]
    fn test_started_state_stop_message() {
        let (client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);
        client_state.state = State::Started;
        let mut ctx = AgentContext {
            another_started: true,
        };

        send_control_message(&client, ControlMessage::Stop);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);
        assert!(matches!(result, MessageResult::Disconnect));
    }

    #[rstest]
    fn test_started_state_no_message() {
        let (_client, server) = create_socket_pair();
        let mut client_state = ClientState::new(1, server);
        client_state.state = State::Started;
        let mut ctx = AgentContext {
            another_started: true,
        };

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Continue));
    }
}
