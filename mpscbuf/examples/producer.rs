use clap::Parser;
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use mpscbuf::{Producer, WakeupStrategy};
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use std::num::NonZeroU32;
use std::os::fd::{AsFd, OwnedFd};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixListener;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, info};

#[derive(Parser, Debug)]
#[clap(name = "producer")]
#[clap(about = "Ring buffer producer example", long_about = None)]
struct Args {
    #[clap(short, long, default_value = "/tmp/mpscbuf_producer")]
    socket_prefix: String,

    #[clap(short, long)]
    id: u32,

    #[clap(short, long, default_value_t = 1000)]
    rate: u32,

    #[clap(short, long, default_value_t = 1024)]
    message_size: usize,

    #[clap(short, long, default_value_t = 1000)]
    print_interval: u64,

    #[clap(short, long, default_value = "forced", value_parser = parse_wakeup_strategy)]
    wakeup_strategy: WakeupStrategy,
}

fn parse_wakeup_strategy(strategy: &str) -> Result<WakeupStrategy, String> {
    match strategy.to_lowercase().as_str() {
        "forced" => Ok(WakeupStrategy::Forced),
        "no-wakeup" | "nowakeup" => Ok(WakeupStrategy::NoWakeup),
        _ => Err(format!(
            "Invalid wakeup strategy: {}. Valid options: forced, no-wakeup",
            strategy
        )),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!(producer_id = args.id, "starting producer");
    debug!(?args, "producer configuration");

    let socket_path = format!("{}_{}.sock", args.socket_prefix, args.id);
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    info!(producer_id = args.id, socket_path = %socket_path, "producer listening");

    let (stream, _) = listener.accept()?;
    info!(producer_id = args.id, "consumer connected");

    // Receive memory size and file descriptors in one message
    let mut cmsg_buffer = nix::cmsg_space!([std::os::fd::RawFd; 2]);

    let mut size_buf = [0u8; 8];
    let mut iov: [std::io::IoSliceMut<'_>; 1] = [std::io::IoSliceMut::new(&mut size_buf)];
    let (memory_size, msg) = {
        let msg = recvmsg::<()>(
            stream.as_fd().as_raw_fd(),
            &mut iov,
            Some(&mut cmsg_buffer),
            MsgFlags::empty(),
        )?;
        let first = msg.iovs().next().unwrap();
        let memory_size = usize::from_le_bytes(first[..8].try_into().unwrap());

        (memory_size, msg)
    };

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

    let memory_fd = memory_fd.ok_or("failed to receive memory fd")?;
    let notification_fd = notification_fd.ok_or("failed to receive notification fd")?;

    info!(
        producer_id = args.id,
        memory_fd = memory_fd.as_raw_fd(),
        notification_fd = notification_fd.as_raw_fd(),
        memory_size = memory_size,
        "received connection info"
    );

    let wakeup_strategy = args.wakeup_strategy;

    let producer = Producer::new(memory_fd, notification_fd, memory_size, wakeup_strategy)?;

    info!(
        producer_id = args.id,
        wakeup_strategy = ?wakeup_strategy,
        "producer configured with wakeup strategy"
    );

    let messages_per_second = NonZeroU32::new(args.rate).unwrap();
    let quota = Quota::per_second(messages_per_second);
    let rate_limiter = Arc::new(RateLimiter::<
        NotKeyed,
        InMemoryState,
        DefaultClock,
        NoOpMiddleware,
    >::direct(quota));

    let mut sequence = 0u64;
    let message_size = args.message_size;
    let start_time = Instant::now();

    info!(
        producer_id = args.id,
        rate = args.rate,
        message_size = message_size,
        "starting message production"
    );

    loop {
        rate_limiter
            .check()
            .map_err(|_| {
                thread::sleep(Duration::from_millis(1));
            })
            .ok();

        let timestamp = start_time.elapsed().as_nanos() as u64;

        let mut message_data = Vec::with_capacity(24 + message_size);
        message_data.extend_from_slice(&timestamp.to_le_bytes());
        message_data.extend_from_slice(&sequence.to_le_bytes());
        message_data.extend_from_slice(&args.id.to_le_bytes());
        message_data.resize(24 + message_size, b'a');

        match producer.reserve(message_data.len()) {
            Ok(mut reserved) => {
                reserved.copy_from_slice(&message_data);
                drop(reserved);

                if matches!(wakeup_strategy, WakeupStrategy::NoWakeup) {
                    let _ = producer.notify();
                }

                sequence += 1;

                debug!(
                    producer_id = args.id,
                    sequence = sequence,
                    timestamp = timestamp,
                    "message sent"
                );

                if sequence % args.print_interval == 0 {
                    info!(
                        producer_id = args.id,
                        messages_sent = sequence,
                        elapsed_secs = start_time.elapsed().as_secs(),
                        "progress update"
                    );
                }
            }
            Err(e) => {
                debug!(
                    producer_id = args.id,
                    error = %e,
                    "failed to reserve space, retrying"
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
