use mpscbuf::{Producer as MpscProducer, WakeupStrategy};
use protocol::{ArchivedTracingArgs, ControlMessage, TimestampType};
use std::collections::{HashMap, VecDeque};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::extension::{AgentHandle, Extension, ExtensionError};
use crate::{AgentError, Producer};

#[derive(Debug)]
pub(crate) enum Action {
    SendMessage {
        client_id: u64,
        message: ControlMessage,
    },
    DeleteEpoll {
        client_id: u64,
    },
}

pub(crate) struct AgentContext {
    pub(crate) another_started: bool,
}

pub(crate) const CLIENT_TIMEOUT_NS: u64 = 2_000_000_000;

#[derive(Clone)]
pub(crate) enum State {
    Pending,
    Started,
}

pub(crate) struct AgentClientState {
    state: State,
    timestamp_ns: u64,
}

impl AgentClientState {
    pub(crate) fn new(_client_id: u64) -> Self {
        AgentClientState {
            state: State::Pending,
            timestamp_ns: get_timestamp_ns(),
        }
    }

    pub(crate) fn timer(&self, timeout_ns: u64) -> bool {
        let current_time = get_timestamp_ns();
        let elapsed = current_time.saturating_sub(self.timestamp_ns);
        elapsed <= timeout_ns
    }
}

pub(crate) enum MessageResult {
    Continue,
    Started,
    Disconnect,
    #[allow(dead_code)]
    Error(AgentError),
}

pub(crate) struct AgentState {
    clients: HashMap<u64, AgentClientState>,
    started_client: Option<u64>,
    next_client_id: u64,
    ctx: AgentContext,
    timeout_ns: u64,
    tracing_extension: Option<Box<dyn Extension<Args = ArchivedTracingArgs>>>,
    extension_active: bool,
}

impl AgentState {
    pub(crate) fn new(
        timeout_ms: u16,
        tracing_extension: Option<Box<dyn Extension<Args = ArchivedTracingArgs>>>,
    ) -> Self {
        AgentState {
            clients: HashMap::new(),
            started_client: None,
            next_client_id: 1,
            ctx: AgentContext {
                another_started: false,
            },
            timeout_ns: timeout_ms as u64 * 1_000_000,
            tracing_extension,
            extension_active: false,
        }
    }

    pub(crate) fn remove_client(&mut self, token: u64) {
        self.clients.remove(&token);
    }

    pub(crate) fn get_next_client_id(&mut self) -> u64 {
        let id = self.next_client_id;
        self.next_client_id += 1;
        id
    }

    pub(crate) fn timeout_ms(&self) -> u16 {
        (self.timeout_ns / 1_000_000) as u16
    }

    pub(crate) fn register_client(&mut self, client_id: u64, _actions: &mut VecDeque<Action>) {
        let client = AgentClientState::new(client_id);
        self.clients.insert(client_id, client);
        debug!(client_id, "client registered");
    }

    pub(crate) fn handle_disconnect(&mut self, client_id: u64, actions: &mut VecDeque<Action>) {
        if self.started_client == Some(client_id) {
            if self.extension_active {
                if let Some(ref ext) = self.tracing_extension {
                    match ext.stop() {
                        Ok(()) => debug!(client_id, "extension stopped successfully"),
                        Err(e) => debug!(client_id, error = ?e, "extension stop failed"),
                    }
                    self.extension_active = false;
                }
            }

            self.ctx.another_started = false;
            self.started_client = None;
        }
        actions.push_back(Action::DeleteEpoll { client_id });
    }

    pub(crate) fn handle_message(
        &mut self,
        client_id: u64,
        archived_msg: &protocol::ArchivedControlMessage,
        raw_fds: &[RawFd],
        actions: &mut VecDeque<Action>,
    ) {
        if let Some(client) = self.clients.get_mut(&client_id) {
            let current_state = client.state.clone();

            let result = match current_state {
                State::Pending => match archived_msg {
                    protocol::ArchivedControlMessage::Start { buffer_size, args } => {
                        if self.ctx.another_started {
                            let error_msg = "another client already started";
                            actions.push_back(Action::SendMessage {
                                client_id,
                                message: ControlMessage::Nack {
                                    error: error_msg.to_owned(),
                                },
                            });
                            debug!("queued nack - another client already started");
                            return;
                        }

                        let buffer_size = buffer_size.to_native() as usize;

                        let _deserialized_args = match args {
                            protocol::ArchivedArgs::Tracing(tracing_args) => {
                                let timestamp_type_native = match tracing_args.timestamp_type {
                                    protocol::ArchivedTimestampType::Monotonic => {
                                        TimestampType::Monotonic
                                    }
                                    protocol::ArchivedTimestampType::Boottime => {
                                        TimestampType::Boottime
                                    }
                                    protocol::ArchivedTimestampType::Realtime => {
                                        TimestampType::Realtime
                                    }
                                };
                                let log_filter = tracing_args.log_filter.as_str().to_string();

                                Some(protocol::Args::Tracing(protocol::TracingArgs {
                                    log_filter,
                                    timestamp_type: timestamp_type_native,
                                }))
                            }
                        };

                        if raw_fds.len() < 2 {
                            actions.push_back(Action::SendMessage {
                                client_id,
                                message: ControlMessage::Nack {
                                    error: "missing file descriptors".to_owned(),
                                },
                            });
                            return;
                        }

                        if raw_fds[0] < 0 || raw_fds[1] < 0 {
                            actions.push_back(Action::SendMessage {
                                client_id,
                                message: ControlMessage::Nack {
                                    error: "invalid file descriptors".to_owned(),
                                },
                            });
                            return;
                        }

                        let memory_fd = unsafe { OwnedFd::from_raw_fd(raw_fds[0]) };
                        let notification_fd = unsafe { OwnedFd::from_raw_fd(raw_fds[1]) };

                        match MpscProducer::new(
                            memory_fd,
                            notification_fd,
                            buffer_size,
                            WakeupStrategy::NoWakeup,
                        ) {
                            Ok(new_producer) => {
                                let producer = Arc::new(Producer::from_inner(new_producer));

                                if let Ok(exe_path) = std::env::current_exe() {
                                    let process_name = exe_path
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("unknown");

                                    let process_event =
                                        protocol::Event::ProcessName(protocol::ProcessName {
                                            name: process_name,
                                            pid: std::process::id() as i32,
                                        });

                                    if let Err(e) = producer.submit(&process_event) {
                                        debug!(error = ?e, "failed to submit process name event");
                                    }
                                }

                                if let Some(ref ext) = self.tracing_extension {
                                    let protocol::ArchivedArgs::Tracing(tracing_args) = args;
                                    let handle = AgentHandle::new(producer.clone());
                                    debug!(client_id, "starting extension");
                                    match ext.start(tracing_args, &handle) {
                                        Ok(()) => {
                                            self.extension_active = true;
                                            debug!(client_id, "extension started successfully");
                                        }
                                        Err(e) => {
                                            let ExtensionError::ValidationError(error_msg) = e;
                                            actions.push_back(Action::SendMessage {
                                                client_id,
                                                message: ControlMessage::Nack {
                                                    error: error_msg.clone(),
                                                },
                                            });
                                            debug!(client_id, error = %error_msg, "extension start failed");
                                            return;
                                        }
                                    }
                                }

                                debug!(buffer_size, client_id, "producer initialized");

                                actions.push_back(Action::SendMessage {
                                    client_id,
                                    message: ControlMessage::Ack,
                                });
                                debug!("queued ack - producer initialized");

                                MessageResult::Started
                            }
                            Err(e) => {
                                warn!(client_id, error = ?e, "failed to create producer");
                                MessageResult::Error(e.into())
                            }
                        }
                    }
                    _ => {
                        debug!(client_id, "unexpected message in pending state");
                        MessageResult::Continue
                    }
                },
                State::Started => match archived_msg {
                    protocol::ArchivedControlMessage::Stop => {
                        debug!(client_id, "client sent stop");
                        actions.push_back(Action::SendMessage {
                            client_id,
                            message: ControlMessage::Ack,
                        });
                        MessageResult::Disconnect
                    }
                    protocol::ArchivedControlMessage::Continue => {
                        client.timestamp_ns = get_timestamp_ns();
                        debug!(client_id, "client sent continue");
                        actions.push_back(Action::SendMessage {
                            client_id,
                            message: ControlMessage::Ack,
                        });
                        MessageResult::Continue
                    }
                    _ => {
                        debug!(client_id, "unexpected message in started state");
                        MessageResult::Continue
                    }
                },
            };

            match result {
                MessageResult::Continue => {}
                MessageResult::Started => {
                    self.ctx.another_started = true;
                    self.started_client = Some(client_id);
                    client.state = State::Started;
                    client.timestamp_ns = get_timestamp_ns();
                }
                MessageResult::Disconnect | MessageResult::Error(_) => {
                    self.handle_disconnect(client_id, actions);
                }
            }
        }
    }

    pub(crate) fn timer(&mut self, actions: &mut VecDeque<Action>) {
        debug!(
            pending_clients = self.clients.len(),
            has_started_client = self.started_client.is_some(),
            "timer check invoked"
        );

        for (client_id, client) in self.clients.iter() {
            if Some(*client_id) == self.started_client {
                continue;
            }

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
                actions.push_back(Action::DeleteEpoll {
                    client_id: *client_id,
                });
            }
        }

        if let Some(client_id) = &self.started_client {
            let is_alive = self.clients.get(client_id).unwrap().timer(self.timeout_ns);
            debug!(
                client_id,
                is_alive,
                timeout_ns = self.timeout_ns,
                "checking started client timer"
            );
            if !is_alive {
                debug!(
                    client_id,
                    timeout_s = self.timeout_ns / 1_000_000_000,
                    "started client timed out, disconnecting"
                );
                if self.clients.contains_key(client_id) {
                    actions.push_back(Action::DeleteEpoll {
                        client_id: *client_id,
                    });
                }
                if self.extension_active {
                    if let Some(ref ext) = self.tracing_extension {
                        match ext.stop() {
                            Ok(()) => debug!(client_id, "extension stopped successfully"),
                            Err(e) => debug!(client_id, error = ?e, "extension stop failed"),
                        }
                        self.extension_active = false;
                    }
                }
                self.started_client = None;
                self.ctx.another_started = false;
            }
        }
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
    use protocol::ControlMessage;
    use rstest::{fixture, rstest};
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Once;
    use tempfile::TempDir;

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
        Consumer::new(4096 * 4096).unwrap()
    }

    #[fixture]
    fn agent_context() -> AgentContext {
        AgentContext {
            another_started: false,
        }
    }

    #[rstest]
    fn test_agent_client_state_new() {
        let client_id = 42;

        let client_state = AgentClientState::new(client_id);

        assert!(matches!(client_state.state, State::Pending));
        assert!(client_state.timestamp_ns > 0);
    }

    #[rstest]
    fn test_agent_client_state_timer_within_timeout() {
        let client_state = AgentClientState::new(1);

        assert!(client_state.timer(2_000_000_000));
    }

    #[rstest]
    fn test_agent_client_state_timer_after_timeout() {
        let mut client_state = AgentClientState::new(1);
        let timeout_ns = 2_000_000_000;

        client_state.timestamp_ns = get_timestamp_ns() - timeout_ns - 1;

        assert!(!client_state.timer(timeout_ns));
    }

    #[rstest]
    fn test_agent_state_new() {
        let agent_state = AgentState::new(2000, None);

        assert!(agent_state.clients.is_empty());
        assert!(agent_state.started_client.is_none());
        assert_eq!(agent_state.next_client_id, 1);
        assert!(!agent_state.ctx.another_started);
        assert_eq!(agent_state.timeout_ns, 2_000_000_000);
    }

    #[rstest]
    fn test_register_client() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        assert_eq!(client_id, 1);
        assert_eq!(agent_state.next_client_id, 2);
        assert!(agent_state.clients.contains_key(&1));
    }

    #[rstest]
    fn test_accept_multiple_connections() {
        let mut agent_state = AgentState::new(2000, None);

        let temp_dir1 = TempDir::new().unwrap();
        let socket_path1 = temp_dir1.path().join("test1.sock");
        let listener1 = UnixListener::bind(&socket_path1).unwrap();
        let client1 = UnixStream::connect(&socket_path1).unwrap();
        let (server1, _) = listener1.accept().unwrap();
        client1.set_nonblocking(true).unwrap();
        server1.set_nonblocking(true).unwrap();

        let temp_dir2 = TempDir::new().unwrap();
        let socket_path2 = temp_dir2.path().join("test2.sock");
        let listener2 = UnixListener::bind(&socket_path2).unwrap();
        let client2 = UnixStream::connect(&socket_path2).unwrap();
        let (server2, _) = listener2.accept().unwrap();
        client2.set_nonblocking(true).unwrap();
        server2.set_nonblocking(true).unwrap();

        let mut actions = VecDeque::new();
        let client_id1 = agent_state.get_next_client_id();
        agent_state.register_client(client_id1, &mut actions);

        let client_id2 = agent_state.get_next_client_id();
        agent_state.register_client(client_id2, &mut actions);

        assert_eq!(client_id1, 1);
        assert_eq!(client_id2, 2);
        assert_eq!(agent_state.next_client_id, 3);
        assert_eq!(agent_state.clients.len(), 2);
    }

    #[rstest]
    fn test_timer_no_clients() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        agent_state.timer(&mut actions);

        assert!(actions.is_empty());
    }

    #[rstest]
    fn test_timer_pending_client_not_timed_out() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let mut actions = VecDeque::new();
        agent_state.timer(&mut actions);

        assert!(actions.is_empty());
        assert!(agent_state.clients.contains_key(&client_id));
    }

    #[rstest]
    fn test_handle_disconnect() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);
        agent_state.started_client = Some(client_id);
        agent_state.ctx.another_started = true;

        let mut actions = VecDeque::new();
        agent_state.handle_disconnect(client_id, &mut actions);

        assert!(!agent_state.ctx.another_started);
        assert!(agent_state.started_client.is_none());
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::DeleteEpoll { client_id: id } if id == client_id));
    }

    #[rstest]
    fn test_handle_message_pending_start_success(test_consumer: Consumer) {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let start_msg = ControlMessage::Start {
            buffer_size: 4096,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "debug".to_string(),
                timestamp_type: protocol::TimestampType::Monotonic,
            }),
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();

        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let memory_fd = test_consumer.memory_fd().try_clone_to_owned().unwrap();
        let notification_fd = test_consumer
            .notification_fd()
            .try_clone_to_owned()
            .unwrap();
        let raw_fds = [memory_fd.into_raw_fd(), notification_fd.into_raw_fd()];

        let mut actions = VecDeque::new();
        agent_state.handle_message(client_id, archived_msg, &raw_fds, &mut actions);

        assert!(agent_state.ctx.another_started);
        assert_eq!(agent_state.started_client, Some(client_id));
        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], Action::SendMessage { client_id: id, message: ControlMessage::Ack } if *id == client_id)
        );

        let client = agent_state.clients.get(&client_id).unwrap();
        assert!(matches!(client.state, State::Started));
    }

    #[rstest]
    fn test_handle_message_pending_start_another_started(test_consumer: Consumer) {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        agent_state.ctx.another_started = true;

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let start_msg = ControlMessage::Start {
            buffer_size: 4096,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "debug".to_string(),
                timestamp_type: protocol::TimestampType::Monotonic,
            }),
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let memory_fd = test_consumer.memory_fd().try_clone_to_owned().unwrap();
        let notification_fd = test_consumer
            .notification_fd()
            .try_clone_to_owned()
            .unwrap();
        let raw_fds = [memory_fd.into_raw_fd(), notification_fd.into_raw_fd()];

        let mut actions = VecDeque::new();
        agent_state.handle_message(client_id, archived_msg, &raw_fds, &mut actions);

        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], Action::SendMessage { client_id: id, message: ControlMessage::Nack { error } }
            if *id == client_id && *error == "another client already started")
        );

        let client = agent_state.clients.get(&client_id).unwrap();
        assert!(matches!(client.state, State::Pending));
    }

    #[rstest]
    fn test_handle_message_pending_start_invalid_filter(test_consumer: Consumer) {
        use crate::extension::{AgentHandle, Extension, ExtensionError};
        use tracing_subscriber::EnvFilter;

        struct ValidatingExtension;

        impl Extension for ValidatingExtension {
            type Args = protocol::ArchivedTracingArgs;

            fn start(
                &self,
                args: &protocol::ArchivedTracingArgs,
                _handle: &AgentHandle,
            ) -> Result<(), ExtensionError> {
                let log_filter = args.log_filter.as_str();
                EnvFilter::try_new(log_filter).map_err(|_| {
                    ExtensionError::ValidationError("invalid log filter".to_string())
                })?;
                Ok(())
            }

            fn stop(&self) -> Result<(), ExtensionError> {
                Ok(())
            }
        }

        let mut agent_state = AgentState::new(2000, Some(Box::new(ValidatingExtension)));
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let start_msg = ControlMessage::Start {
            buffer_size: 4096,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "invalid!!!filter".to_string(),
                timestamp_type: protocol::TimestampType::Monotonic,
            }),
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let memory_fd = test_consumer.memory_fd().try_clone_to_owned().unwrap();
        let notification_fd = test_consumer
            .notification_fd()
            .try_clone_to_owned()
            .unwrap();
        let raw_fds = [memory_fd.into_raw_fd(), notification_fd.into_raw_fd()];

        let mut actions = VecDeque::new();
        agent_state.handle_message(client_id, archived_msg, &raw_fds, &mut actions);

        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], Action::SendMessage { client_id: id, message: ControlMessage::Nack { error } }
            if *id == client_id && error == "invalid log filter")
        );
    }

    #[rstest]
    fn test_handle_message_pending_start_missing_fds() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let start_msg = ControlMessage::Start {
            buffer_size: 4096,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "debug".to_string(),
                timestamp_type: protocol::TimestampType::Monotonic,
            }),
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let raw_fds = [];

        let mut actions = VecDeque::new();
        agent_state.handle_message(client_id, archived_msg, &raw_fds, &mut actions);

        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], Action::SendMessage { client_id: id, message: ControlMessage::Nack { error } }
            if *id == client_id && *error == "missing file descriptors")
        );
    }

    #[rstest]
    fn test_handle_message_started_continue() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let client = agent_state.clients.get_mut(&client_id).unwrap();
        client.state = State::Started;
        let old_timestamp = client.timestamp_ns;

        std::thread::sleep(std::time::Duration::from_millis(1));

        let continue_msg = protocol::ControlMessage::Continue;
        let serialized_len = protocol::compute_length(&continue_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&continue_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let mut actions = VecDeque::new();
        agent_state.handle_message(client_id, archived_msg, &[], &mut actions);

        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], Action::SendMessage { client_id: id, message: ControlMessage::Ack } if *id == client_id)
        );

        let client = agent_state.clients.get(&client_id).unwrap();
        assert!(client.timestamp_ns > old_timestamp);
    }

    #[rstest]
    fn test_handle_message_started_stop() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);
        agent_state.started_client = Some(client_id);
        agent_state.ctx.another_started = true;

        let client = agent_state.clients.get_mut(&client_id).unwrap();
        client.state = State::Started;

        let stop_msg = protocol::ControlMessage::Stop;
        let serialized_len = protocol::compute_length(&stop_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&stop_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let mut actions = VecDeque::new();
        agent_state.handle_message(client_id, archived_msg, &[], &mut actions);

        assert_eq!(actions.len(), 2);
        assert!(
            matches!(&actions[0], Action::SendMessage { client_id: id, message: ControlMessage::Ack } if *id == client_id)
        );
        assert!(matches!(&actions[1], Action::DeleteEpoll { client_id: id } if *id == client_id));

        assert!(!agent_state.ctx.another_started);
        assert!(agent_state.started_client.is_none());
    }

    #[rstest]
    fn test_timer_pending_client_timeout() {
        let mut agent_state = AgentState::new(2, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let client = agent_state.clients.get_mut(&client_id).unwrap();
        client.timestamp_ns = get_timestamp_ns() - 3_000_000_000;

        let mut actions = VecDeque::new();
        agent_state.timer(&mut actions);

        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::DeleteEpoll { client_id: id } if *id == client_id));
    }

    #[rstest]
    fn test_timer_started_client_timeout() {
        let mut agent_state = AgentState::new(2, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);
        agent_state.started_client = Some(client_id);
        agent_state.ctx.another_started = true;

        let client = agent_state.clients.get_mut(&client_id).unwrap();
        client.state = State::Started;
        client.timestamp_ns = get_timestamp_ns() - 3_000_000_000;

        let mut actions = VecDeque::new();
        agent_state.timer(&mut actions);

        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::DeleteEpoll { client_id: id } if *id == client_id));
        assert!(agent_state.started_client.is_none());
        assert!(!agent_state.ctx.another_started);
    }

    #[rstest]
    fn test_full_client_lifecycle(test_consumer: Consumer) {
        init_tracing();
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);
        assert_eq!(actions.len(), 0);
        assert!(agent_state.clients.contains_key(&client_id));

        let start_msg = ControlMessage::Start {
            buffer_size: 4096,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "info".to_string(),
                timestamp_type: protocol::TimestampType::Boottime,
            }),
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let memory_fd = test_consumer.memory_fd().try_clone_to_owned().unwrap();
        let notification_fd = test_consumer
            .notification_fd()
            .try_clone_to_owned()
            .unwrap();
        let raw_fds = [memory_fd.into_raw_fd(), notification_fd.into_raw_fd()];

        agent_state.handle_message(client_id, archived_msg, &raw_fds, &mut actions);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::SendMessage {
                message: ControlMessage::Ack,
                ..
            }
        ));
        assert!(agent_state.ctx.another_started);
        assert_eq!(agent_state.started_client, Some(client_id));

        for _ in 0..3 {
            actions.clear();
            std::thread::sleep(std::time::Duration::from_millis(1));
            let continue_msg = protocol::ControlMessage::Continue;
            let serialized_len = protocol::compute_length(&continue_msg).unwrap();
            let mut buf = vec![0u8; serialized_len];
            protocol::serialize_to_buf(&continue_msg, &mut buf).unwrap();
            let archived_msg =
                rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf)
                    .unwrap();
            agent_state.handle_message(client_id, archived_msg, &[], &mut actions);
            assert_eq!(actions.len(), 1);
            assert!(matches!(
                &actions[0],
                Action::SendMessage {
                    message: ControlMessage::Ack,
                    ..
                }
            ));
        }

        actions.clear();
        let stop_msg = protocol::ControlMessage::Stop;
        let serialized_len = protocol::compute_length(&stop_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&stop_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();
        agent_state.handle_message(client_id, archived_msg, &[], &mut actions);
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            Action::SendMessage {
                message: ControlMessage::Ack,
                ..
            }
        ));
        assert!(matches!(&actions[1], Action::DeleteEpoll { .. }));

        assert!(!agent_state.ctx.another_started);
        assert!(agent_state.started_client.is_none());
    }

    #[rstest]
    fn test_multiple_clients_only_one_starts(test_consumer: Consumer) {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client1_id = agent_state.get_next_client_id();
        agent_state.register_client(client1_id, &mut actions);

        let client2_id = agent_state.get_next_client_id();
        agent_state.register_client(client2_id, &mut actions);

        let client3_id = agent_state.get_next_client_id();
        agent_state.register_client(client3_id, &mut actions);

        assert_eq!(agent_state.clients.len(), 3);

        let start_msg = ControlMessage::Start {
            buffer_size: 4096,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "debug".to_string(),
                timestamp_type: protocol::TimestampType::Monotonic,
            }),
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let memory_fd = test_consumer.memory_fd().try_clone_to_owned().unwrap();
        let notification_fd = test_consumer
            .notification_fd()
            .try_clone_to_owned()
            .unwrap();
        let raw_fds = [memory_fd.into_raw_fd(), notification_fd.into_raw_fd()];

        actions.clear();
        agent_state.handle_message(client1_id, archived_msg, &raw_fds, &mut actions);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::SendMessage {
                message: ControlMessage::Ack,
                ..
            }
        ));

        actions.clear();
        agent_state.handle_message(client2_id, archived_msg, &raw_fds, &mut actions);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::SendMessage {
                message: ControlMessage::Nack { .. },
                ..
            }
        ));

        actions.clear();
        agent_state.handle_message(client3_id, archived_msg, &raw_fds, &mut actions);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            Action::SendMessage {
                message: ControlMessage::Nack { .. },
                ..
            }
        ));

        assert_eq!(agent_state.started_client, Some(client1_id));
        let client1 = agent_state.clients.get(&client1_id).unwrap();
        assert!(matches!(client1.state, State::Started));

        let client2 = agent_state.clients.get(&client2_id).unwrap();
        assert!(matches!(client2.state, State::Pending));

        let client3 = agent_state.clients.get(&client3_id).unwrap();
        assert!(matches!(client3.state, State::Pending));
    }

    #[rstest]
    fn test_invalid_fd_handling() {
        let mut agent_state = AgentState::new(2000, None);
        let mut actions = VecDeque::new();

        let client_id = agent_state.get_next_client_id();
        agent_state.register_client(client_id, &mut actions);

        let start_msg = ControlMessage::Start {
            buffer_size: 4096,
            args: protocol::Args::Tracing(protocol::TracingArgs {
                log_filter: "debug".to_string(),
                timestamp_type: protocol::TimestampType::Monotonic,
            }),
        };

        let serialized_len = protocol::compute_length(&start_msg).unwrap();
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf).unwrap();
        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&buf).unwrap();

        let raw_fds = [-1, -2];
        actions.clear();
        agent_state.handle_message(client_id, archived_msg, &raw_fds, &mut actions);

        assert_eq!(actions.len(), 1);
        assert!(
            matches!(&actions[0], Action::SendMessage { message: ControlMessage::Nack { error }, .. }
            if *error == "invalid file descriptors")
        );
    }
}
