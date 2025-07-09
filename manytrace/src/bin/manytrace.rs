use agent::{AgentClient, Consumer};
use clap::Parser;
use eyre::{Context, Result};
use manytrace::config::Config;
use manytrace::converter::PerfettoConverter;
use std::cell::RefCell;
use std::fs::File;
use std::io::BufWriter;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

static LONG_VERSION: OnceLock<String> = OnceLock::new();

fn get_long_version() -> &'static str {
    LONG_VERSION.get_or_init(|| {
        format!(
            "{} (commit: {}, protocol: {})",
            env!("CARGO_PKG_VERSION"),
            env!("GIT_REVISION", "unknown"),
            protocol::VERSION
        )
    })
}

#[derive(Parser)]
#[command(name = "manytrace")]
#[command(about = "unified bpf and user-space tracing tool")]
#[command(version = None, long_version = get_long_version())]
struct Args {
    #[arg(help = "configuration file path (toml format)")]
    config: String,

    #[arg(
        short,
        long,
        default_value = "trace.perfetto",
        help = "output file for trace data"
    )]
    output: String,

    #[arg(
        short,
        long,
        value_parser = humantime::parse_duration,
        help = "duration to collect events (e.g. 10s, 5m, 1h)"
    )]
    duration: Option<Duration>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::load(&args.config)
        .with_context(|| format!("failed to load config path={}", args.config))?;

    let running = Arc::new(AtomicBool::new(true));
    let stopped = Arc::new(AtomicBool::new(false));
    let r = running.clone();
    let s = stopped.clone();
    ctrlc::set_handler(move || {
        tracing::info!("received ctrl+c, shutting down gracefully...");
        r.store(false, Ordering::SeqCst);
        s.store(true, Ordering::SeqCst);
    })?;

    let file = File::create(&args.output)?;
    let buffered_writer = BufWriter::new(file);
    let converter = Rc::new(RefCell::new(PerfettoConverter::new(buffered_writer)));

    let mut bpf_object = config.bpf.build()?;
    let callback = {
        let converter = converter.clone();
        move |message: bpf::BpfMessage| {
            if let Err(e) = converter.borrow_mut().convert_message(&message) {
                tracing::warn!(error = %e, "failed to convert bpf event");
            }
        }
    };
    let mut stream_allocator = protocol::StreamIdAllocator::new();
    let mut bpf_consumer = bpf_object.consumer(callback, &mut stream_allocator, stopped.clone())?;

    let mut user_consumer = if !config.user.is_empty() {
        Some(Consumer::new(config.global.buffer_size)?)
    } else {
        None
    };

    let mut user_clients = Vec::new();
    if let Some(ref consumer) = user_consumer {
        for user_config in &config.user {
            let mut client = AgentClient::new(user_config.socket.clone());

            let start_result = if let Some(random_process_id) = user_config.random_process_id {
                let mut options = std::collections::HashMap::new();
                options.insert(
                    agent::RANDOM_PROCESS_ID_OPTION,
                    protocol::Value::Bool(random_process_id),
                );
                client.start_with_options(
                    consumer,
                    user_config.log_filter.clone(),
                    protocol::TimestampType::Monotonic,
                    options,
                )
            } else {
                client.start(consumer, user_config.log_filter.clone())
            };

            match start_result {
                Ok(_) => {
                    tracing::info!(socket = %user_config.socket, "connected to user agent");
                    user_clients.push(client);
                }
                Err(e) => {
                    tracing::error!(socket = %user_config.socket, error = %e, "failed to connect");
                    for client in &mut user_clients {
                        if let Err(stop_err) = client.stop() {
                            tracing::warn!(error = %stop_err, "failed to stop client during cleanup");
                        }
                    }
                    return Err(e.into());
                }
            }
        }
    }

    let start_time = Instant::now();
    let duration = args.duration;
    let mut last_keepalive = Instant::now();

    while running.load(Ordering::SeqCst) && duration.is_none_or(|d| start_time.elapsed() < d) {
        if last_keepalive.elapsed() >= Duration::from_secs(1) {
            for client in &mut user_clients {
                if let Err(e) = client.send_continue() {
                    tracing::warn!(error = %e, "failed to send keepalive");
                }
            }
            last_keepalive = Instant::now();
        }

        if let Some(ref mut consumer) = user_consumer {
            while let Some(record) = consumer.consume() {
                match record.as_event() {
                    Ok(event) => {
                        if let Err(e) = converter.borrow_mut().convert_archived_event(event, None) {
                            tracing::warn!(error = %e, "failed to convert user event");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "failed to deserialize event"),
                }
            }
        }

        if let Err(e) = bpf_consumer.consume() {
            tracing::warn!(error = %e, "bpf consume error");
        }

        sleep(Duration::from_millis(10));
    }

    drop(bpf_consumer);
    drop(bpf_object);

    converter.borrow_mut().flush()?;

    tracing::info!(
        output = %args.output,
        "trace collection complete"
    );
    Ok(())
}
