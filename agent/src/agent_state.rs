use arc_swap::ArcSwapOption;
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use tracing::debug;

use crate::state::{AgentContext, ClientState, MessageResult, CLIENT_TIMEOUT_NS};
use crate::{Producer, Result};

pub enum EpollAction<'a> {
    Add { fd: &'a UnixStream, token: u64 },
    Delete { fd: UnixStream },
}

pub struct AgentState {
    pending_clients: HashMap<u64, ClientState>,
    started_client: Option<(u64, ClientState)>,
    next_client_id: u64,
    ctx: AgentContext,
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentState {
    pub fn new() -> Self {
        AgentState {
            pending_clients: HashMap::new(),
            started_client: None,
            next_client_id: 1,
            ctx: AgentContext {
                another_started: false,
            },
        }
    }

    pub fn accept_connection(&mut self, stream: UnixStream) -> Result<(u64, EpollAction)> {
        let client_id = self.next_client_id;
        self.next_client_id += 1;

        stream.set_nonblocking(true)?;

        self.pending_clients
            .insert(client_id, ClientState::new(client_id, stream));

        debug!(client_id, "client connected, waiting for start message");

        Ok((
            client_id,
            EpollAction::Add {
                fd: self.pending_clients.get(&client_id).unwrap().stream(),
                token: client_id,
            },
        ))
    }

    pub fn handle_event(
        &mut self,
        event_data: u64,
        producer: &Arc<ArcSwapOption<Producer>>,
    ) -> Option<EpollAction> {
        if let Some(mut client) = self.pending_clients.remove(&event_data) {
            match client.handle_message(&mut self.ctx) {
                MessageResult::Continue => {
                    self.pending_clients.insert(event_data, client);
                    None
                }
                MessageResult::Started(new_producer) => {
                    producer.store(Some(new_producer));
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
                        MessageResult::Continue => None,
                        MessageResult::Started(_) => None,
                        MessageResult::Disconnect | MessageResult::Error(_) => {
                            let fd = client.into_stream();
                            producer.store(None);
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

    pub fn timer(&mut self, producer: &Arc<ArcSwapOption<Producer>>) -> Vec<EpollAction> {
        let mut actions = Vec::new();

        let mut timed_out_clients = Vec::new();
        for (client_id, client) in self.pending_clients.iter() {
            if !client.timer() {
                debug!(
                    client_id,
                    timeout_s = CLIENT_TIMEOUT_NS / 1_000_000_000,
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
            if !client.timer() {
                debug!(
                    client_id,
                    timeout_s = CLIENT_TIMEOUT_NS / 1_000_000_000,
                    "started client timed out, disconnecting"
                );
                if let Some((_, client)) = self.started_client.take() {
                    actions.push(EpollAction::Delete {
                        fd: client.into_stream(),
                    });
                }
                self.started_client = None;
                producer.store(None);
                self.ctx.another_started = false;
            }
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwapOption;
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

    #[rstest]
    fn test_agent_state_new() {
        let agent_state = AgentState::new();

        assert!(agent_state.pending_clients.is_empty());
        assert!(agent_state.started_client.is_none());
        assert_eq!(agent_state.next_client_id, 1);
        assert!(!agent_state.ctx.another_started);
    }

    #[rstest]
    fn test_agent_state_default() {
        let agent_state = AgentState::default();

        assert!(agent_state.pending_clients.is_empty());
        assert!(agent_state.started_client.is_none());
        assert_eq!(agent_state.next_client_id, 1);
        assert!(!agent_state.ctx.another_started);
    }

    #[rstest]
    fn test_accept_connection() {
        let mut agent_state = AgentState::new();
        let (_client, server) = create_socket_pair();

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
        let mut agent_state = AgentState::new();
        let (_client1, server1) = create_socket_pair();
        let (_client2, server2) = create_socket_pair();

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
        let mut agent_state = AgentState::new();
        let producer = Arc::new(ArcSwapOption::empty());

        let result = agent_state.handle_event(999, &producer);

        assert!(result.is_none());
    }

    #[rstest]
    fn test_handle_event_pending_client_continue() {
        let mut agent_state = AgentState::new();
        let (_client, server) = create_socket_pair();
        let producer = Arc::new(ArcSwapOption::empty());

        let (client_id, _) = agent_state.accept_connection(server).unwrap();

        let result = agent_state.handle_event(client_id, &producer);

        assert!(result.is_none());
        assert!(agent_state.pending_clients.contains_key(&client_id));
    }

    #[rstest]
    fn test_timer_no_clients() {
        let mut agent_state = AgentState::new();
        let producer = Arc::new(ArcSwapOption::empty());

        let actions = agent_state.timer(&producer);

        assert!(actions.is_empty());
    }

    #[rstest]
    fn test_timer_pending_client_not_timed_out() {
        let mut agent_state = AgentState::new();
        let (_client, server) = create_socket_pair();
        let producer = Arc::new(ArcSwapOption::empty());

        let (client_id, _) = agent_state.accept_connection(server).unwrap();

        let actions = agent_state.timer(&producer);

        assert!(actions.is_empty());
        assert!(agent_state.pending_clients.contains_key(&client_id));
    }
}
