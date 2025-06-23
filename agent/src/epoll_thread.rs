use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::socket::{recvmsg, sendmsg, ControlMessageOwned, MsgFlags};
use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::agent_state::{Action, AgentState};
use crate::Result;

const LISTENER_TOKEN: u64 = 0;

pub fn epoll_listener_thread(
    socket_path: String,
    shutdown: Arc<AtomicBool>,
    mut agent_state: AgentState,
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
    let timeout = EpollTimeout::from(agent_state.timeout_ms());
    let mut client_sockets: HashMap<u64, UnixStream> = HashMap::new();

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
                        stream.set_nonblocking(true)?;

                        let client_id = agent_state.get_next_client_id();
                        let mut actions = VecDeque::new();

                        epoll.add(&stream, EpollEvent::new(EpollFlags::EPOLLIN, client_id))?;
                        client_sockets.insert(client_id, stream);
                        agent_state.register_client(client_id, &mut actions);

                        for token in process_actions(&mut actions, &epoll, &client_sockets)? {
                            agent_state.remove_client(token);
                            if let Some(socket) = client_sockets.remove(&token) {
                                epoll.delete(&socket)?;
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        warn!(error = ?e, "error accepting connection");
                    }
                },
                client_id if client_id != LISTENER_TOKEN => {
                    if let Some(stream) = client_sockets.get(&client_id) {
                        let mut cmsg_buffer = nix::cmsg_space!([RawFd; 2]);
                        let mut msg_buf = [0u8; 1024];
                        let mut iov = [std::io::IoSliceMut::new(&mut msg_buf)];

                        match recvmsg::<()>(
                            stream.as_raw_fd(),
                            &mut iov,
                            Some(&mut cmsg_buffer),
                            MsgFlags::empty(),
                        ) {
                            Ok(msg) => {
                                if let Some(data) = msg.iovs().next() {
                                    if !data.is_empty() {
                                        let mut raw_fds: Vec<RawFd> = Vec::new();

                                        if let Ok(cmsgs) = msg.cmsgs() {
                                            for cmsg in cmsgs {
                                                if let ControlMessageOwned::ScmRights(fds) = cmsg {
                                                    raw_fds.extend_from_slice(fds.as_slice());
                                                }
                                            }
                                        }

                                        match rkyv::access::<
                                            protocol::ArchivedControlMessage,
                                            rkyv::rancor::Error,
                                        >(data)
                                        {
                                            Ok(archived_msg) => {
                                                let mut actions = VecDeque::new();
                                                agent_state.handle_message(
                                                    client_id,
                                                    archived_msg,
                                                    &raw_fds,
                                                    &mut actions,
                                                );
                                                for token in process_actions(
                                                    &mut actions,
                                                    &epoll,
                                                    &client_sockets,
                                                )? {
                                                    agent_state.remove_client(token);
                                                    if let Some(socket) =
                                                        client_sockets.remove(&token)
                                                    {
                                                        epoll.delete(&socket)?;
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                warn!(client_id, error = ?e, "failed to deserialize message");
                                            }
                                        }
                                    }
                                }
                            }
                            Err(nix::errno::Errno::EAGAIN) => {}
                            Err(e) => {
                                warn!(error = ?e, client_id, "error reading from client");
                                let mut actions = VecDeque::new();
                                agent_state.handle_disconnect(client_id, &mut actions);
                                for token in process_actions(&mut actions, &epoll, &client_sockets)?
                                {
                                    agent_state.remove_client(token);
                                    if let Some(socket) = client_sockets.remove(&token) {
                                        epoll.delete(&socket)?;
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let mut actions = VecDeque::new();
        agent_state.timer(&mut actions);
        for token in process_actions(&mut actions, &epoll, &client_sockets)? {
            agent_state.remove_client(token);
            if let Some(socket) = client_sockets.remove(&token) {
                epoll.delete(&socket)?;
            }
        }
    }
    Ok(())
}

fn process_actions(
    actions: &mut VecDeque<Action>,
    _epoll: &Epoll,
    client_sockets: &HashMap<u64, UnixStream>,
) -> Result<Vec<u64>> {
    let mut ids = vec![];
    while let Some(action) = actions.pop_front() {
        match action {
            Action::SendMessage { client_id, message } => {
                if let Some(stream) = client_sockets.get(&client_id) {
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
            }
            Action::DeleteEpoll { client_id } => {
                ids.push(client_id);
            }
        }
    }
    Ok(ids)
}
