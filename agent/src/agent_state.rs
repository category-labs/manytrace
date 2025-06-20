use arc_swap::ArcSwapOption;
use mpscbuf::{Producer as MpscProducer, WakeupStrategy};
use nix::sys::socket::{recvmsg, sendmsg, ControlMessageOwned, MsgFlags};
use protocol::{ControlMessage, TimestampType};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::agent::ProducerState;
use crate::{AgentError, Producer, Result};

pub(crate) struct AgentContext {
    pub(crate) another_started: bool,
}

pub(crate) const CLIENT_TIMEOUT_NS: u64 = 2_000_000_000;

pub(crate) enum State {
    Pending,
    Started,
}

pub(crate) struct AgentClientState {
    client_id: u64,
    stream: UnixStream,
    state: State,
    timestamp_ns: u64,
}

impl AgentClientState {
    pub(crate) fn new(client_id: u64, stream: UnixStream) -> Self {
        AgentClientState {
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

    pub(crate) fn timer(&self, timeout_ns: u64) -> bool {
        let current_time = get_timestamp_ns();
        let elapsed = current_time.saturating_sub(self.timestamp_ns);
        elapsed <= timeout_ns
    }

    pub(crate) fn handle_message(&mut self, ctx: &mut AgentContext) -> MessageResult {
        match &self.state {
            State::Pending => handle_pending_state(self, ctx),
            State::Started => handle_started_state(self),
        }
    }
}

pub(crate) enum MessageResult {
    Continue,
    Started(Arc<ProducerState>),
    Disconnect,
    #[allow(dead_code)]
    Error(AgentError),
}

fn handle_pending_state(
    client_state: &mut AgentClientState,
    ctx: &mut AgentContext,
) -> MessageResult {
    match handle_start_message(&client_state.stream, ctx, client_state.client_id) {
        Ok(Some(producer_state_result)) => {
            client_state.timestamp_ns = get_timestamp_ns();
            client_state.state = State::Started;
            debug!(client_id = client_state.client_id, "client started");
            MessageResult::Started(producer_state_result)
        }
        Ok(None) => MessageResult::Continue,
        Err(e) => {
            warn!(client_id = client_state.client_id, error = ?e, "error handling start message");
            MessageResult::Error(e)
        }
    }
}

fn handle_started_state(client_state: &mut AgentClientState) -> MessageResult {
    match handle_client_message(&client_state.stream) {
        Ok(Some(protocol::ControlMessage::Stop)) => {
            debug!(client_id = client_state.client_id, "client sent stop");
            let ack_msg = ControlMessage::Ack;
            if let Err(e) = send_message(&client_state.stream, &ack_msg) {
                warn!(client_id = client_state.client_id, error = ?e, "failed to send stop ack");
            }
            MessageResult::Disconnect
        }
        Ok(Some(protocol::ControlMessage::Continue)) => {
            client_state.timestamp_ns = get_timestamp_ns();
            debug!(
                client_id = client_state.client_id,
                timestamp = client_state.timestamp_ns,
                "client sent continue"
            );
            let ack_msg = ControlMessage::Ack;
            if let Err(e) = send_message(&client_state.stream, &ack_msg) {
                warn!(client_id = client_state.client_id, error = ?e, "failed to send continue ack");
            }
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
    client_id: u64,
) -> Result<Option<Arc<ProducerState>>> {
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
        protocol::ArchivedControlMessage::Start { buffer_size, args } => {
            if ctx.another_started {
                let error_msg = "another client already started".to_string();
                let nack_msg = ControlMessage::Nack { error: &error_msg };
                send_message(stream, &nack_msg)?;
                debug!("sent nack - another client already started");
                return Ok(None);
            }

            let buffer_size = buffer_size.to_native() as usize;

            let deserialized_args = match args {
                protocol::ArchivedArgs::Tracing(tracing_args) => {
                    let timestamp_type_native = match tracing_args.timestamp_type {
                        protocol::ArchivedTimestampType::Monotonic => TimestampType::Monotonic,
                        protocol::ArchivedTimestampType::Boottime => TimestampType::Boottime,
                        protocol::ArchivedTimestampType::Realtime => TimestampType::Realtime,
                    };
                    let log_filter = tracing_args.log_filter.as_str().to_string();

                    // Validate the log filter before proceeding
                    if let Err(e) = tracing_subscriber::EnvFilter::try_new(&log_filter) {
                        let error_msg = format!("Invalid log filter '{}': {}", log_filter, e);
                        let nack_msg = ControlMessage::Nack { error: &error_msg };
                        send_message(stream, &nack_msg)?;
                        debug!("sent nack - {}", error_msg);
                        return Ok(None);
                    }

                    Some(protocol::Args::Tracing(protocol::TracingArgs {
                        log_filter,
                        timestamp_type: timestamp_type_native,
                    }))
                }
            };

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
                WakeupStrategy::NoWakeup,
            )?;

            let producer = Arc::new(Producer::from_inner(new_producer));

            if let Ok(exe_path) = std::env::current_exe() {
                let process_name = exe_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown");

                let process_event = protocol::Event::ProcessName(protocol::ProcessName {
                    name: process_name,
                    pid: std::process::id() as i32,
                });

                if let Err(e) = producer.submit(&process_event) {
                    debug!(error = ?e, "failed to submit process name event");
                }
            }

            let producer_state = Arc::new(ProducerState::new(producer, deserialized_args));

            debug!(buffer_size, client_id, "producer initialized");

            let ack_msg = ControlMessage::Ack;
            send_message(stream, &ack_msg)?;
            debug!("sent ack - producer initialized");

            Ok(Some(producer_state))
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

pub(crate) enum EpollAction<'a> {
    Add { fd: &'a UnixStream, token: u64 },
    Delete { fd: UnixStream },
}

pub(crate) struct AgentState {
    pending_clients: HashMap<u64, AgentClientState>,
    started_client: Option<(u64, AgentClientState)>,
    next_client_id: u64,
    ctx: AgentContext,
    timeout_ns: u64,
}

impl AgentState {
    pub(crate) fn new(timeout_ms: u16) -> Self {
        AgentState {
            pending_clients: HashMap::new(),
            started_client: None,
            next_client_id: 1,
            ctx: AgentContext {
                another_started: false,
            },
            timeout_ns: timeout_ms as u64 * 1_000_000,
        }
    }

    pub(crate) fn accept_connection(&mut self, stream: UnixStream) -> Result<(u64, EpollAction)> {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        stream.set_nonblocking(true)?;

        self.pending_clients
            .insert(client_id, AgentClientState::new(client_id, stream));

        debug!(client_id, "client connected, waiting for start message");

        Ok((
            client_id,
            EpollAction::Add {
                fd: self.pending_clients.get(&client_id).unwrap().stream(),
                token: client_id,
            },
        ))
    }

    pub(crate) fn handle_event(
        &mut self,
        event_data: u64,
        producer_state: &Arc<ArcSwapOption<ProducerState>>,
    ) -> Option<EpollAction> {
        if let Some(mut client) = self.pending_clients.remove(&event_data) {
            match client.handle_message(&mut self.ctx) {
                MessageResult::Continue => {
                    self.pending_clients.insert(event_data, client);
                    None
                }
                MessageResult::Started(new_producer_state) => {
                    producer_state.store(Some(new_producer_state));
                    self.ctx.another_started = true;
                    self.started_client = Some((event_data, client));
                    None
                }
                MessageResult::Disconnect => {
                    let fd = client.into_stream();
                    Some(EpollAction::Delete { fd })
                }
                MessageResult::Error(_) => {
                    let fd = client.into_stream();
                    Some(EpollAction::Delete { fd })
                }
            }
        } else if let Some((id, _)) = &self.started_client {
            if *id == event_data {
                if let Some((_, mut client)) = self.started_client.take() {
                    match client.handle_message(&mut self.ctx) {
                        MessageResult::Continue => {
                            self.started_client = Some((event_data, client));
                            None
                        }
                        MessageResult::Started(_) => {
                            self.started_client = Some((event_data, client));
                            None
                        }
                        MessageResult::Disconnect | MessageResult::Error(_) => {
                            let fd = client.into_stream();
                            producer_state.store(None);
                            self.ctx.another_started = false;
                            Some(EpollAction::Delete { fd })
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    pub(crate) fn timer(
        &mut self,
        producer_state: &Arc<ArcSwapOption<ProducerState>>,
    ) -> Vec<EpollAction> {
        debug!(
            pending_clients = self.pending_clients.len(),
            has_started_client = self.started_client.is_some(),
            "timer check invoked"
        );
        let mut actions = Vec::new();

        let mut timed_out_clients = Vec::new();
        for (client_id, client) in self.pending_clients.iter() {
            let is_alive = client.timer(self.timeout_ns);
            debug!(
                client_id,
                is_alive,
                elapsed_ns = get_timestamp_ns().saturating_sub(client.timestamp_ns),
                timeout_ns = self.timeout_ns,
                "checking pending client timer"
            );
            if !is_alive {
                debug!(
                    client_id,
                    timeout_s = self.timeout_ns / 1_000_000_000,
                    "pending client timed out, disconnecting"
                );
                timed_out_clients.push(*client_id);
            }
        }

        for client_id in &timed_out_clients {
            if let Some(client) = self.pending_clients.remove(client_id) {
                actions.push(EpollAction::Delete {
                    fd: client.into_stream(),
                });
            }
        }

        for client_id in timed_out_clients {
            self.pending_clients.remove(&client_id);
        }

        if let Some((client_id, client)) = &self.started_client {
            let is_alive = client.timer(self.timeout_ns);
            debug!(
                client_id,
                is_alive,
                elapsed_ns = get_timestamp_ns().saturating_sub(client.timestamp_ns),
                timeout_ns = self.timeout_ns,
                "checking started client timer"
            );
            if !is_alive {
                debug!(
                    client_id,
                    timeout_s = self.timeout_ns / 1_000_000_000,
                    "started client timed out, disconnecting"
                );
                if let Some((_, client)) = self.started_client.take() {
                    actions.push(EpollAction::Delete {
                        fd: client.into_stream(),
                    });
                }
                self.started_client = None;
                producer_state.store(None);
                self.ctx.another_started = false;
            }
        }

        actions
    }
}

fn get_timestamp_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Consumer;
    use arc_swap::ArcSwapOption;
    use protocol::ControlMessage;
    use rstest::{fixture, rstest};
    use std::os::unix::net::{UnixListener, UnixStream};
    use tempfile::TempDir;

    #[fixture]
    fn socket_pair() -> (UnixStream, UnixStream) {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let client = UnixStream::connect(&socket_path).unwrap();
        let (server, _) = listener.accept().unwrap();

        client.set_nonblocking(true).unwrap();
        server.set_nonblocking(true).unwrap();

        (client, server)
    }

    #[fixture]
    fn test_consumer() -> Consumer {
        Consumer::new(1024 * 1024).unwrap()
    }

    #[fixture]
    fn agent_context() -> AgentContext {
        AgentContext {
            another_started: false,
        }
    }

    fn send_start_message(stream: &UnixStream, buffer_size: u64, log_filter: &str) {
        let start_msg = ControlMessage::Start {
            buffer_size,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: log_filter.to_string(),
                timestamp_type: protocol::TimestampType::Monotonic,
            }),
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
        send_start_message_with_fds_and_timestamp(
            stream,
            consumer,
            protocol::TimestampType::Monotonic,
        );
    }

    fn send_start_message_with_fds_and_timestamp(
        stream: &UnixStream,
        consumer: &Consumer,
        timestamp_type: protocol::TimestampType,
    ) {
        let start_msg = ControlMessage::Start {
            buffer_size: consumer.data_size() as u64,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "debug".to_string(),
                timestamp_type,
            }),
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
    fn test_agent_client_state_new(socket_pair: (UnixStream, UnixStream)) {
        let (_client, server) = socket_pair;
        let client_id = 42;

        let client_state = AgentClientState::new(client_id, server);

        assert_eq!(client_state.client_id, 42);
        assert!(matches!(client_state.state, State::Pending));
        assert!(client_state.timestamp_ns > 0);
    }

    #[rstest]
    fn test_agent_client_state_timer_within_timeout(socket_pair: (UnixStream, UnixStream)) {
        let (_client, server) = socket_pair;
        let client_state = AgentClientState::new(1, server);

        assert!(client_state.timer(2_000_000_000));
    }

    #[rstest]
    fn test_agent_client_state_timer_after_timeout(socket_pair: (UnixStream, UnixStream)) {
        let (_client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);
        let timeout_ns = 2_000_000_000;

        client_state.timestamp_ns = get_timestamp_ns() - timeout_ns - 1;

        assert!(!client_state.timer(timeout_ns));
    }

    #[rstest]
    fn test_pending_state_no_message(
        socket_pair: (UnixStream, UnixStream),
        mut agent_context: AgentContext,
    ) {
        let (_client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);

        let result = client_state.handle_message(&mut agent_context);

        assert!(matches!(result, MessageResult::Continue));
        assert!(matches!(client_state.state, State::Pending));
    }

    #[rstest]
    fn test_pending_state_with_start_no_fds(socket_pair: (UnixStream, UnixStream)) {
        let (client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);
        let mut ctx = AgentContext {
            another_started: false,
        };

        send_start_message(&client, 1024, "debug");
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Error(_)));
    }

    #[rstest]
    fn test_pending_state_successful_start(
        socket_pair: (UnixStream, UnixStream),
        test_consumer: Consumer,
    ) {
        let (client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);
        let mut ctx = AgentContext {
            another_started: false,
        };

        send_start_message_with_fds(&client, &test_consumer);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Started(_)));
        assert!(matches!(client_state.state, State::Started));
    }

    #[rstest]
    fn test_pending_state_start_with_existing_producer(
        socket_pair: (UnixStream, UnixStream),
        test_consumer: Consumer,
    ) {
        let (client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);
        let mut ctx = AgentContext {
            another_started: true,
        };

        send_start_message_with_fds(&client, &test_consumer);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Continue));
        assert!(matches!(client_state.state, State::Pending));
    }

    #[rstest]
    fn test_started_state_continue_message(socket_pair: (UnixStream, UnixStream)) {
        let (client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);
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
    fn test_started_state_stop_message(socket_pair: (UnixStream, UnixStream)) {
        let (client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);
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
    fn test_started_state_no_message(socket_pair: (UnixStream, UnixStream)) {
        let (_client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);
        client_state.state = State::Started;
        let mut ctx = AgentContext {
            another_started: true,
        };

        let result = client_state.handle_message(&mut ctx);

        assert!(matches!(result, MessageResult::Continue));
    }

    #[rstest]
    fn test_agent_state_new() {
        let agent_state = AgentState::new(2000);

        assert!(agent_state.pending_clients.is_empty());
        assert!(agent_state.started_client.is_none());
        assert_eq!(agent_state.next_client_id, 1);
        assert!(!agent_state.ctx.another_started);
        assert_eq!(agent_state.timeout_ns, 2_000_000_000);
    }

    #[rstest]
    fn test_accept_connection(socket_pair: (UnixStream, UnixStream)) {
        let mut agent_state = AgentState::new(2000);
        let (_client, server) = socket_pair;

        let result = agent_state.accept_connection(server);

        assert!(result.is_ok());
        let (client_id, action) = result.unwrap();
        assert_eq!(client_id, 1);
        assert!(matches!(action, EpollAction::Add { .. }));
        assert_eq!(agent_state.next_client_id, 2);
        assert!(agent_state.pending_clients.contains_key(&1));
    }

    #[rstest]
    fn test_accept_multiple_connections() {
        let mut agent_state = AgentState::new(2000);

        // Create first socket pair
        let temp_dir1 = TempDir::new().unwrap();
        let socket_path1 = temp_dir1.path().join("test1.sock");
        let listener1 = UnixListener::bind(&socket_path1).unwrap();
        let client1 = UnixStream::connect(&socket_path1).unwrap();
        let (server1, _) = listener1.accept().unwrap();
        client1.set_nonblocking(true).unwrap();
        server1.set_nonblocking(true).unwrap();

        // Create second socket pair
        let temp_dir2 = TempDir::new().unwrap();
        let socket_path2 = temp_dir2.path().join("test2.sock");
        let listener2 = UnixListener::bind(&socket_path2).unwrap();
        let client2 = UnixStream::connect(&socket_path2).unwrap();
        let (server2, _) = listener2.accept().unwrap();
        client2.set_nonblocking(true).unwrap();
        server2.set_nonblocking(true).unwrap();

        let result1 = agent_state.accept_connection(server1);
        assert!(result1.is_ok());
        let (client_id1, _) = result1.unwrap();

        let result2 = agent_state.accept_connection(server2);
        assert!(result2.is_ok());
        let (client_id2, _) = result2.unwrap();

        assert_eq!(client_id1, 1);
        assert_eq!(client_id2, 2);
        assert_eq!(agent_state.next_client_id, 3);
        assert_eq!(agent_state.pending_clients.len(), 2);
    }

    #[rstest]
    fn test_handle_event_nonexistent_client() {
        let mut agent_state = AgentState::new(2000);
        let producer = Arc::new(ArcSwapOption::empty());

        let result = agent_state.handle_event(999, &producer);

        assert!(result.is_none());
    }

    #[rstest]
    fn test_handle_event_pending_client_continue(socket_pair: (UnixStream, UnixStream)) {
        let mut agent_state = AgentState::new(2000);
        let (_client, server) = socket_pair;
        let producer = Arc::new(ArcSwapOption::empty());

        let (client_id, _) = agent_state.accept_connection(server).unwrap();

        let result = agent_state.handle_event(client_id, &producer);

        assert!(result.is_none());
        assert!(agent_state.pending_clients.contains_key(&client_id));
    }

    #[rstest]
    fn test_timer_no_clients() {
        let mut agent_state = AgentState::new(2000);
        let producer = Arc::new(ArcSwapOption::empty());

        let actions = agent_state.timer(&producer);

        assert!(actions.is_empty());
    }

    #[rstest]
    fn test_timer_pending_client_not_timed_out(socket_pair: (UnixStream, UnixStream)) {
        let mut agent_state = AgentState::new(2000);
        let (_client, server) = socket_pair;
        let producer = Arc::new(ArcSwapOption::empty());

        let (client_id, _) = agent_state.accept_connection(server).unwrap();

        let actions = agent_state.timer(&producer);

        assert!(actions.is_empty());
        assert!(agent_state.pending_clients.contains_key(&client_id));
    }

    #[rstest]
    #[case::monotonic(protocol::TimestampType::Monotonic)]
    #[case::boottime(protocol::TimestampType::Boottime)]
    #[case::realtime(protocol::TimestampType::Realtime)]
    fn test_timestamp_types(
        socket_pair: (UnixStream, UnixStream),
        test_consumer: Consumer,
        mut agent_context: AgentContext,
        #[case] timestamp_type: protocol::TimestampType,
    ) {
        let (client, server) = socket_pair;
        let mut client_state = AgentClientState::new(1, server);

        send_start_message_with_fds_and_timestamp(&client, &test_consumer, timestamp_type);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let result = client_state.handle_message(&mut agent_context);

        assert!(matches!(result, MessageResult::Started(_)));
        assert!(matches!(client_state.state, State::Started));
    }
}
