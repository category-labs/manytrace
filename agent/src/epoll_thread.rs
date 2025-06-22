use arc_swap::ArcSwapOption;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::socket::{sendmsg, MsgFlags};
use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::agent::ProducerState;
use crate::agent_state::{Action, AgentState};
use crate::Result;

const LISTENER_TOKEN: u64 = 0;

pub fn epoll_listener_thread(
    socket_path: String,
    producer_state: Arc<ArcSwapOption<ProducerState>>,
    shutdown: Arc<AtomicBool>,
    timeout_ms: u16,
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
    let mut agent_state = AgentState::new(timeout_ms, producer_state.clone());
    let timeout = EpollTimeout::from(timeout_ms);

    loop {
        if shutdown.load(Ordering::Relaxed) {
            debug!("agent listener thread shutting down");
            break;
        }
        let nfds = epoll.wait(&mut events, timeout)?;

        for event in events.iter().take(nfds) {
            match event.data() {
                LISTENER_TOKEN => match listener.accept() {
                    Ok((stream, _)) => {
                        debug!("client connected to agent");
                        let mut actions = VecDeque::new();
                        match agent_state.accept_connection(stream, &mut actions) {
                            Ok(_client_id) => {}
                            Err(e) => {
                                warn!(error = ?e, "error setting up client connection");
                            }
                        }

                        for token in process_actions(&mut actions, &epoll)? {
                            agent_state.drop_state(token);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        warn!(error = ?e, "error accepting connection");
                    }
                },
                client_id if client_id != LISTENER_TOKEN => {
                    let mut actions = VecDeque::new();
                    agent_state.handle_event(client_id, &mut actions);
                    for token in process_actions(&mut actions, &epoll)? {
                        agent_state.drop_state(token);
                    }
                }
                _ => {}
            }
        }
        let mut actions = VecDeque::new();
        agent_state.timer(&mut actions);
        for token in process_actions(&mut actions, &epoll)? {
            agent_state.drop_state(token);
        }
    }
    Ok(())
}

fn process_actions(actions: &mut VecDeque<Action>, epoll: &Epoll) -> Result<Vec<u64>> {
    let mut ids = vec![];
    while let Some(action) = actions.pop_front() {
        match action {
            Action::SendMessage { stream, message } => {
                let serialized = protocol::compute_length(&message)?;
                let mut buf = vec![0u8; serialized];
                protocol::serialize_to_buf(&message, &mut buf)?;

                let iov = [std::io::IoSlice::new(&buf)];
                if let Err(e) =
                    sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)
                {
                    warn!(error = ?e, "failed to send message");
                }
            }
            Action::AddEpoll { stream, token } => {
                if let Err(e) = epoll.add(stream, EpollEvent::new(EpollFlags::EPOLLIN, token)) {
                    warn!(error = ?e, "failed to add fd to epoll");
                }
            }
            Action::DeleteEpoll { token, stream } => {
                let _ = epoll.delete(stream);
                ids.push(token);
            }
        }
    }
    Ok(ids)
}
