use arc_swap::ArcSwapOption;
use mpscbuf::{Producer as MpscProducer, WakeupStrategy};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::socket::{recvmsg, sendmsg, ControlMessageOwned, MsgFlags};
use protocol::ControlMessage;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::{get_timestamp_ns, AgentError, Producer, Result};

const LISTENER_TOKEN: u64 = 0;
const CLIENT_TOKEN: u64 = 1;

struct ClientState {
    stream: UnixStream,
    pid: i32,
    keepalive_interval_ns: u64,
}

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

    let mut events = vec![EpollEvent::empty(); 2];
    let mut client_state: Option<ClientState> = None;
    let mut last_continue_ns: u64 = 0;

    loop {
        let timeout_ms = if let Some(ref client) = client_state {
            if client.keepalive_interval_ns > 0 {
                let timeout_ns = client.keepalive_interval_ns * 2;
                (timeout_ns / 1_000_000) as isize
            } else {
                5000
            }
        } else {
            5000
        };

        let timeout = EpollTimeout::from(timeout_ms as u16);

        let nfds = epoll.wait(&mut events, timeout)?;

        for event in events.iter().take(nfds) {
            match event.data() {
                LISTENER_TOKEN => match listener.accept() {
                    Ok((stream, _)) => {
                        debug!("client connected to agent");

                        if client_state.is_some() {
                            debug!(
                                current_pid = client_state.as_ref().unwrap().pid,
                                "rejecting connection - already have active client"
                            );
                            continue;
                        }

                        stream.set_nonblocking(true)?;

                        debug!("handling start message");
                        match handle_start_message(&stream, &producer) {
                            Ok(Some((pid, keepalive_ns))) => {
                                epoll.add(
                                    &stream,
                                    EpollEvent::new(EpollFlags::EPOLLIN, CLIENT_TOKEN),
                                )?;

                                client_state = Some(ClientState {
                                    stream,
                                    pid,
                                    keepalive_interval_ns: keepalive_ns,
                                });
                                last_continue_ns = get_timestamp_ns();
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(error = ?e, "error handling start message");
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        warn!(error = ?e, "error accepting connection");
                    }
                },
                CLIENT_TOKEN => {
                    if let Some(client) = client_state.take() {
                        match handle_client_message(&client.stream) {
                            Ok(Some(protocol::ControlMessage::Stop)) => {
                                debug!(pid = client.pid, "client sent stop");
                                let _ = epoll.delete(&client.stream);
                                cleanup_client(&producer);
                                last_continue_ns = 0;
                            }
                            Ok(Some(protocol::ControlMessage::Continue)) => {
                                last_continue_ns = get_timestamp_ns();
                                debug!(
                                    pid = client.pid,
                                    timestamp = last_continue_ns,
                                    "client sent continue"
                                );
                                client_state = Some(client);
                            }
                            Ok(Some(_)) => {
                                debug!("unexpected message from client");
                                client_state = Some(client);
                            }
                            Ok(None) => {
                                debug!(pid = client.pid, "client disconnected");
                                let _ = epoll.delete(&client.stream);
                                cleanup_client(&producer);
                                last_continue_ns = 0;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                client_state = Some(client);
                            }
                            Err(e) => {
                                warn!(error = ?e, "error handling client message");
                                let _ = epoll.delete(&client.stream);
                                cleanup_client(&producer);
                                last_continue_ns = 0;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(client) = client_state.as_ref() {
            let current_time = get_timestamp_ns();
            let timeout_ns = client.keepalive_interval_ns * 2;

            if current_time.saturating_sub(last_continue_ns) > timeout_ns {
                debug!(
                    pid = client.pid,
                    elapsed_ns = current_time.saturating_sub(last_continue_ns),
                    timeout_ns,
                    "client timed out, disconnecting"
                );
                let client = client_state.take().unwrap();
                let _ = epoll.delete(&client.stream);
                cleanup_client(&producer);
                last_continue_ns = 0;
            }
        }
    }
}

fn handle_start_message(
    stream: &UnixStream,
    producer: &Arc<ArcSwapOption<Producer>>,
) -> Result<Option<(i32, u64)>> {
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
        protocol::ArchivedControlMessage::Start {
            buffer_size,
            keepalive_interval_ns: archived_keepalive_interval_ns,
            pid: archived_pid,
            force: archived_force,
            ..
        } => {
            let pid = archived_pid.to_native();
            let force = *archived_force;
            let keepalive_ns = archived_keepalive_interval_ns.to_native();

            if producer.load().is_some() && !force {
                let current_pid = 0; // We don't track PID anymore in epoll version
                let error_msg = format!("producer already exists for pid {}", current_pid);
                let nack_msg = ControlMessage::Nack { error: &error_msg };
                send_message(stream, &nack_msg)?;
                debug!(
                    requesting_pid = pid,
                    current_pid, "sent nack - producer already exists"
                );
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

            producer.store(Some(Arc::new(Producer::from_inner(new_producer))));

            debug!(
                pid,
                buffer_size,
                keepalive_interval_ns = keepalive_ns,
                "producer initialized"
            );

            let ack_msg = ControlMessage::Ack;
            send_message(stream, &ack_msg)?;
            debug!("sent ack - producer initialized");

            Ok(Some((pid, keepalive_ns)))
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

fn cleanup_client(producer: &Arc<ArcSwapOption<Producer>>) {
    producer.store(None);
    debug!("producer cleared");
}
