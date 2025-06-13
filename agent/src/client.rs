use mpscbuf::Consumer;
use nix::sys::socket::{recv, sendmsg, ControlMessage, MsgFlags};
use protocol::{ControlMessage as ProtocolControlMessage, LogLevel};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use tracing::debug;

use crate::{AgentError, Result};

pub struct AgentClient {
    socket_path: String,
    stream: Option<UnixStream>,
}

impl AgentClient {
    pub fn new(socket_path: String) -> Self {
        AgentClient {
            socket_path,
            stream: None,
        }
    }

    pub fn start(&mut self, consumer: &Consumer, log_level: LogLevel) -> Result<()> {
        let stream = UnixStream::connect(&self.socket_path)?;

        let start_msg = ProtocolControlMessage::Start {
            buffer_size: consumer.data_size() as u64,
            log_level,
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

        let mut response_buf = [0u8; 1024];
        let bytes_read = recv(stream.as_raw_fd(), &mut response_buf, MsgFlags::empty())?;

        let archived_response = rkyv::access::<
            protocol::ArchivedControlMessage,
            rkyv::rancor::Error,
        >(&response_buf[..bytes_read])?;

        match archived_response {
            protocol::ArchivedControlMessage::Ack => {
                debug!("received ack from agent");
                self.stream = Some(stream);
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
            sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)?;

            debug!("sent stop message to agent");
            self.stream = None;
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
            sendmsg::<()>(stream.as_raw_fd(), &iov, &[], MsgFlags::empty(), None)?;

            debug!("sent continue message to agent");
        }
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.stream.is_some()
    }
}
