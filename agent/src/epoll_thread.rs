use arc_swap::ArcSwapOption;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use tracing::{debug, warn};

use crate::agent_state::{AgentState, EpollAction};
use crate::state::CLIENT_TIMEOUT_NS;
use crate::{Producer, Result};

const LISTENER_TOKEN: u64 = 0;

pub fn epoll_listener_thread(
    socket_path: String,
    producer: Arc<ArcSwapOption<Producer>>,
) -> Result<()> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(true)?;
    debug!(socket_path = %socket_path, "agent listening on unix socket");

    let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?;

    epoll.add(
        &listener,
        EpollEvent::new(EpollFlags::EPOLLIN, LISTENER_TOKEN),
    )?;

    let mut events = vec![EpollEvent::empty(); 10];
    let mut agent_state = AgentState::new();
    let timeout_ms = (CLIENT_TIMEOUT_NS / 1_000_000) as u16;
    let timeout = EpollTimeout::from(timeout_ms);
    loop {
        let nfds = epoll.wait(&mut events, timeout)?;
        for event in events.iter().take(nfds) {
            match event.data() {
                LISTENER_TOKEN => match listener.accept() {
                    Ok((stream, _)) => {
                        debug!("client connected to agent");

                        match agent_state.accept_connection(stream) {
                            Ok((_client_id, EpollAction::Add { fd, token })) => {
                                epoll.add(fd, EpollEvent::new(EpollFlags::EPOLLIN, token))?;
                            }
                            Ok((_, EpollAction::Delete { .. })) => {}
                            Err(e) => {
                                warn!(error = ?e, "error setting up client connection");
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        warn!(error = ?e, "error accepting connection");
                    }
                },
                client_id if client_id != LISTENER_TOKEN => {
                    if let Some(action) = agent_state.handle_event(client_id, &producer) {
                        match action {
                            EpollAction::Add { fd, token } => {
                                epoll.add(fd, EpollEvent::new(EpollFlags::EPOLLIN, token))?;
                            }
                            EpollAction::Delete { fd } => {
                                let _ = epoll.delete(fd);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let timeout_actions = agent_state.timer(&producer);
        for action in timeout_actions {
            match action {
                EpollAction::Add { .. } => {}
                EpollAction::Delete { fd } => {
                    let _ = epoll.delete(fd);
                }
            }
        }
    }
}
