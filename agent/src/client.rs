use crate::Consumer;
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::socket::{recv, sendmsg, ControlMessage, MsgFlags};
use protocol::{Args, ControlMessage as ProtocolControlMessage, TimestampType, TracingArgs};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use tracing::debug;

use crate::{AgentError, Result};

const CLIENT_READ_TIMEOUT_MS: u16 = 5000;

struct ResponseBuffer {
    data: Vec<u8>,
}

impl ResponseBuffer {
    fn as_archived(&self) -> Result<&protocol::ArchivedControlMessage> {
        rkyv::access::<protocol::ArchivedControlMessage, rkyv::rancor::Error>(&self.data)
            .map_err(AgentError::Archive)
    }
}

/// Client that connects to an agent and sends events.
pub struct AgentClient {
    socket_path: String,
    stream: Option<UnixStream>,
}

impl AgentClient {
    fn wait_for_response(stream: &UnixStream) -> Result<ResponseBuffer> {
        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?;
        epoll.add(stream, EpollEvent::new(EpollFlags::EPOLLIN, 0))?;

        let mut events = vec![EpollEvent::empty(); 1];
        let timeout = EpollTimeout::from(CLIENT_READ_TIMEOUT_MS);

        let nfds = epoll.wait(&mut events, timeout)?;
        if nfds == 0 {
            return Err(AgentError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timeout waiting for agent response",
            )));
        }

        let mut response_buf = [0u8; 1024];
        let bytes_read = match recv(stream.as_raw_fd(), &mut response_buf, MsgFlags::empty()) {
            Ok(n) => n,
            Err(nix::errno::Errno::EAGAIN) => {
                return Err(AgentError::Io(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "no data available",
                )))
            }
            Err(e) => return Err(e.into()),
        };

        let mut data = vec![0u8; bytes_read];
        data.copy_from_slice(&response_buf[..bytes_read]);
        Ok(ResponseBuffer { data })
    }
}

impl AgentClient {
    pub fn new(socket_path: String) -> Self {
        AgentClient {
            socket_path,
            stream: None,
        }
    }

    fn connect_or_reuse(&mut self) -> Result<&mut UnixStream> {
        if self.stream.is_none() {
            let stream = UnixStream::connect(&self.socket_path)?;
            stream.set_nonblocking(true)?;
            self.stream = Some(stream);
        }
        self.stream
            .as_mut()
            .ok_or_else(|| AgentError::Io(std::io::Error::other("failed to get stream")))
    }

    pub fn start(&mut self, consumer: &Consumer, log_filter: String) -> Result<()> {
        self.start_with_timestamp(consumer, log_filter, TimestampType::Monotonic)
    }

    pub fn start_with_timestamp(
        &mut self,
        consumer: &Consumer,
        log_filter: String,
        timestamp_type: TimestampType,
    ) -> Result<()> {
        let version = self.check_version()?;
        if version != protocol::VERSION {
            self.stream = None;
            return Err(AgentError::Io(std::io::Error::other(format!(
                "version mismatch: agent version {} != client version {}",
                version,
                protocol::VERSION
            ))));
        }

        let stream = self.connect_or_reuse()?;

        let start_msg = ProtocolControlMessage::Start {
            buffer_size: consumer.data_size() as u64,
            args: Args {
                tracing: Some(TracingArgs {
                    log_filter,
                    timestamp_type,
                }),
            },
        };

        let serialized_len = protocol::compute_length(&start_msg)?;
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&start_msg, &mut buf)?;

        let size_iov = [std::io::IoSlice::new(&buf)];
        let fds = [
            consumer.memory_fd().as_raw_fd(),
            consumer.notification_fd().as_raw_fd(),
        ];
        let cmsg = ControlMessage::ScmRights(&fds);

        sendmsg::<()>(
            stream.as_raw_fd(),
            &size_iov,
            &[cmsg],
            MsgFlags::empty(),
            None,
        )?;

        let response = Self::wait_for_response(stream)?;
        let archived_response = response.as_archived()?;

        match archived_response {
            protocol::ArchivedControlMessage::Ack => {
                debug!("received ack from agent");
                Ok(())
            }
            protocol::ArchivedControlMessage::Nack { error } => {
                let error_str = std::str::from_utf8(error.as_bytes()).unwrap_or("unknown error");
                Err(AgentError::Io(std::io::Error::other(format!(
                    "agent rejected start: {}",
                    error_str
                ))))
            }
            _ => Err(AgentError::Io(std::io::Error::other(
                "unexpected response from agent",
            ))),
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        if let Some(stream) = &self.stream {
            let stop_msg = ProtocolControlMessage::Stop;
            let serialized_len = protocol::compute_length(&stop_msg)?;
            let mut buf = vec![0u8; serialized_len];
            protocol::serialize_to_buf(&stop_msg, &mut buf)?;

            let iov = [std::io::IoSlice::new(&buf)];
            match sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None) {
                Ok(_) => {
                    debug!("sent stop message to agent");
                    match Self::wait_for_response(stream) {
                        Ok(response) => {
                            if let Ok(archived_response) = response.as_archived() {
                                match archived_response {
                                    protocol::ArchivedControlMessage::Ack => {
                                        debug!("received stop ack from agent");
                                    }
                                    _ => {
                                        debug!("unexpected response from agent on stop");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            debug!(error = ?e, "failed to receive stop ack");
                        }
                    }
                    self.stream = None;
                }
                Err(nix::errno::Errno::EAGAIN) => {
                    return Err(AgentError::Io(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "stop message would block",
                    )))
                }
                Err(nix::errno::Errno::EPIPE) => {
                    debug!("connection already closed");
                    self.stream = None;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn send_continue(&self) -> Result<()> {
        if let Some(stream) = &self.stream {
            let continue_msg = ProtocolControlMessage::Continue;
            let serialized_len = protocol::compute_length(&continue_msg)?;
            let mut buf = vec![0u8; serialized_len];
            protocol::serialize_to_buf(&continue_msg, &mut buf)?;

            let iov = [std::io::IoSlice::new(&buf)];
            match sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None) {
                Ok(_) => {
                    debug!("sent continue message to agent");
                    let response = Self::wait_for_response(stream)?;
                    let archived_response = response.as_archived()?;

                    match archived_response {
                        protocol::ArchivedControlMessage::Ack => {
                            debug!("received continue ack from agent");
                        }
                        protocol::ArchivedControlMessage::Nack { error } => {
                            let error_str =
                                std::str::from_utf8(error.as_bytes()).unwrap_or("unknown error");
                            return Err(AgentError::Io(std::io::Error::other(error_str)));
                        }
                        _ => {
                            return Err(AgentError::Io(std::io::Error::other(
                                "unexpected response from agent",
                            )));
                        }
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => {
                    return Err(AgentError::Io(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "continue message would block",
                    )))
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.stream.is_some()
    }

    pub fn keepalive(&self) -> Result<()> {
        if self.enabled() {
            self.send_continue()
        } else {
            Err(AgentError::NotEnabled)
        }
    }

    pub fn check_version(&mut self) -> Result<&'static str> {
        let stream = self.connect_or_reuse()?;

        let version_msg = ProtocolControlMessage::Version;
        let serialized_len = protocol::compute_length(&version_msg)?;
        let mut buf = vec![0u8; serialized_len];
        protocol::serialize_to_buf(&version_msg, &mut buf)?;

        let iov = [std::io::IoSlice::new(&buf)];
        sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)?;

        let response = Self::wait_for_response(stream)?;
        let archived_response = response.as_archived()?;

        match archived_response {
            protocol::ArchivedControlMessage::VersionResponse { version: _ } => {
                Ok(protocol::VERSION)
            }
            _ => Err(AgentError::Io(std::io::Error::other(
                "unexpected response from agent",
            ))),
        }
    }
}
