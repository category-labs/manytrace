use arc_swap::ArcSwapOption;
use mpscbuf::{Producer as MpscProducer, WakeupStrategy};
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use protocol::ControlMessage;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, warn};

use crate::{get_timestamp_ns, AgentError, Producer, Result};

pub fn socket_listener_thread(
    socket_path: String,
    producer: Arc<ArcSwapOption<Producer>>,
    last_continue_ns: Arc<AtomicU64>,
    keepalive_interval_ns: Arc<AtomicU64>,
) -> Result<()> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)?;
    debug!(socket_path = %socket_path, "agent listening on unix socket");

    let mut current_client_pid: i32 = 0;

    loop {
        match listener.accept() {
            Ok((stream, socket)) => {
                debug!(?socket, "client connected to agent");

                match handle_client_connection(
                    &stream,
                    &producer,
                    &last_continue_ns,
                    &keepalive_interval_ns,
                    &mut current_client_pid,
                ) {
                    Ok(()) => debug!("client disconnected successfully"),
                    Err(e) => warn!(error = ?e, "error handling client connection"),
                }
            }
            Err(e) => {
                warn!(error = ?e, "error accepting connection");
                continue;
            }
        }
    }
}

fn handle_client_connection(
    stream: &std::os::unix::net::UnixStream,
    producer: &Arc<ArcSwapOption<Producer>>,
    last_continue_ns: &Arc<AtomicU64>,
    keepalive_interval_ns: &Arc<AtomicU64>,
    current_client_pid: &mut i32,
) -> Result<()> {
    let mut cmsg_buffer = nix::cmsg_space!([std::os::fd::RawFd; 2]);
    let mut msg_buf = [0u8; 1024];
    let mut iov: [std::io::IoSliceMut<'_>; 1] = [std::io::IoSliceMut::new(&mut msg_buf)];

    let msg = recvmsg::<()>(
        stream.as_fd().as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buffer),
        MsgFlags::empty(),
    )?;

    let received_data = msg.iovs().next().unwrap();
    let bytes_read = received_data.len();

    if bytes_read == 0 {
        debug!("client disconnected");
        return Ok(());
    }

    let archived_msg =
        rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(received_data)?;

    debug!("received control message");

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

            if producer.load().is_some() && !force {
                let error_msg = format!("producer already exists for pid {}", *current_client_pid);
                let nack_msg = ControlMessage::Nack { error: &error_msg };
                let serialized = protocol::compute_length(&nack_msg)?;
                let mut buf = vec![0u8; serialized];
                protocol::serialize_to_buf(&nack_msg, &mut buf)?;

                use nix::sys::socket::send;
                send(stream.as_fd().as_raw_fd(), &buf, MsgFlags::empty())?;
                debug!(
                    "sent nack - producer already exists for pid {}",
                    *current_client_pid
                );
                return Ok(());
            }

            let buffer_size = buffer_size.to_native() as usize;

            let mut memory_fd: Option<OwnedFd> = None;
            let mut notification_fd: Option<OwnedFd> = None;

            let cmsgs = msg.cmsgs()?;
            for cmsg in cmsgs {
                match cmsg {
                    ControlMessageOwned::ScmRights(fds) => {
                        if fds.len() >= 2 {
                            memory_fd = Some(unsafe { OwnedFd::from_raw_fd(fds[0]) });
                            notification_fd = Some(unsafe { OwnedFd::from_raw_fd(fds[1]) });
                            break;
                        }
                    }
                    _ => continue,
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

            debug!(
                memory_fd = memory_fd.as_raw_fd(),
                notification_fd = notification_fd.as_raw_fd(),
                buffer_size = buffer_size,
                "received connection info from client"
            );

            let new_producer = MpscProducer::new(
                memory_fd,
                notification_fd,
                buffer_size,
                WakeupStrategy::Forced,
            )?;

            producer.store(Some(Arc::new(Producer::from_inner(new_producer))));
            last_continue_ns.store(get_timestamp_ns(), Ordering::Relaxed);
            keepalive_interval_ns.store(
                archived_keepalive_interval_ns.to_native(),
                Ordering::Relaxed,
            );
            *current_client_pid = pid;
            debug!("producer initialized for pid {}", pid);

            let ack_msg = ControlMessage::Ack;
            let serialized = protocol::compute_length(&ack_msg)?;
            let mut buf = vec![0u8; serialized];
            protocol::serialize_to_buf(&ack_msg, &mut buf)?;

            use nix::sys::socket::send;
            send(stream.as_fd().as_raw_fd(), &buf, MsgFlags::empty())?;
            debug!("sent ack - producer initialized");

            std::thread::sleep(std::time::Duration::from_millis(5));

            let producer_clone = producer.clone();
            let last_continue_clone = last_continue_ns.clone();
            let stream_clone = stream.try_clone()?;
            std::thread::Builder::new()
                .name("agent-handler".to_string())
                .spawn(move || {
                    if let Err(e) =
                        handle_agent_connection(stream_clone, producer_clone, last_continue_clone)
                    {
                        warn!(error = ?e, "agent handler thread error");
                    }
                })?;

            Ok(())
        }
        _ => {
            debug!("expected Start message as first message, dropping connection");
            Ok(())
        }
    }
}

fn handle_agent_connection(
    stream: std::os::unix::net::UnixStream,
    producer: Arc<ArcSwapOption<Producer>>,
    last_continue_ns: Arc<AtomicU64>,
) -> Result<()> {
    loop {
        let mut cmsg_buffer = nix::cmsg_space!([std::os::fd::RawFd; 2]);
        let mut msg_buf = [0u8; 1024];
        let mut iov: [std::io::IoSliceMut<'_>; 1] = [std::io::IoSliceMut::new(&mut msg_buf)];

        let msg = recvmsg::<()>(
            stream.as_fd().as_raw_fd(),
            &mut iov,
            Some(&mut cmsg_buffer),
            MsgFlags::empty(),
        )?;

        let received_data = msg.iovs().next().unwrap();
        let bytes_read = received_data.len();

        if bytes_read == 0 {
            debug!("agent client disconnected");
            break;
        }

        let archived_msg =
            rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(received_data)?;

        debug!("received agent control message");

        match archived_msg {
            protocol::ArchivedControlMessage::Stop => {
                producer.store(None);
                last_continue_ns.store(0, Ordering::Relaxed);
                debug!("producer cleared on stop message");
                break;
            }
            protocol::ArchivedControlMessage::Continue => {
                let timestamp = get_timestamp_ns();
                last_continue_ns.store(timestamp, Ordering::Relaxed);
                debug!(timestamp = timestamp, "received continue message");
            }
            _ => {
                debug!("received unhandled agent control message");
            }
        }
    }

    Ok(())
}
