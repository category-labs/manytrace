use agent::{AgentClient, Consumer};
use chrome_trace_format::{
    ChromeTrace, CompleteEvent, CounterEvent, InstantEvent, InstantScope, MetadataEvent,
    MetadataName, Phase, TraceEvent,
};
use clap::Parser;
use eyre::Result;
use protocol::{ArchivedEvent, LogLevel};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info};

#[derive(Parser, Debug)]
#[command(name = "usertrace")]
#[command(about = "Collect trace events and convert to Chrome trace format", long_about = None)]
struct Args {
    #[arg(short, long, help = "Output file path")]
    output: PathBuf,
    #[arg(
        short,
        long,
        default_value = "/tmp/manytrace.sock",
        help = "Agent socket path"
    )]
    socket: String,
    #[arg(short, long, default_value = "8388608", help = "Buffer size in bytes")]
    buffer_size: usize,
    #[arg(
        short,
        long,
        default_value = "5",
        help = "Duration to collect events in seconds"
    )]
    duration: u64,
    #[arg(
        long,
        default_value = "info",
        help = "Log level (error, warn, info, debug, trace)"
    )]
    log_level: String,
}

struct TraceConverter {
    events: Vec<TraceEvent>,
}

impl TraceConverter {
    fn new() -> Self {
        Self { events: Vec::new() }
    }

    fn convert_event(&mut self, event: &ArchivedEvent) {
        match event {
            ArchivedEvent::Counter(counter) => self.add_counter(counter),
            ArchivedEvent::Span(span) => self.add_span(span),
            ArchivedEvent::Instant(instant) => self.add_instant(instant),
            ArchivedEvent::ThreadName(thread_name) => self.add_thread_name(thread_name),
            ArchivedEvent::ProcessName(process_name) => self.add_process_name(process_name),
        }
    }

    fn add_counter(&mut self, counter: &protocol::ArchivedCounter) {
        let mut args = serde_json::Map::new();
        args.insert(
            counter.name.to_string(),
            serde_json::Value::from(counter.value.to_native()),
        );
        for (key, value) in counter.labels.strings.iter() {
            args.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        for (key, value) in counter.labels.ints.iter() {
            args.insert(key.to_string(), serde_json::Value::from(value.to_native()));
        }
        for (key, value) in counter.labels.floats.iter() {
            args.insert(key.to_string(), serde_json::Value::from(value.to_native()));
        }
        for (key, value) in counter.labels.bools.iter() {
            args.insert(key.to_string(), serde_json::Value::from(*value));
        }
        self.events.push(TraceEvent::Counter(
            CounterEvent::builder()
                .name(counter.name.to_string())
                .cat("counter".to_string())
                .ph(Phase::Counter)
                .ts(counter.timestamp.to_native())
                .pid(counter.pid.to_native() as u32)
                .tid(counter.tid.to_native() as u32)
                .args(serde_json::Value::Object(args))
                .build(),
        ));
    }

    fn add_span(&mut self, span: &protocol::ArchivedSpan) {
        let mut args = serde_json::Map::new();
        for (key, value) in span.labels.strings.iter() {
            args.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        for (key, value) in span.labels.ints.iter() {
            args.insert(key.to_string(), serde_json::Value::from(value.to_native()));
        }
        for (key, value) in span.labels.floats.iter() {
            args.insert(key.to_string(), serde_json::Value::from(value.to_native()));
        }
        for (key, value) in span.labels.bools.iter() {
            args.insert(key.to_string(), serde_json::Value::from(*value));
        }
        let args_value = if args.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(args))
        };
        let start_ts = span.start_timestamp.to_native();
        let end_ts = span.end_timestamp.to_native();
        if end_ts > start_ts {
            self.events.push(TraceEvent::Complete(
                CompleteEvent::builder()
                    .name(span.name.to_string())
                    .cat("span".to_string())
                    .ph(Phase::Complete)
                    .ts(start_ts)
                    .dur(end_ts - start_ts)
                    .pid(span.pid.to_native() as u32)
                    .tid(span.tid.to_native() as u32)
                    .maybe_args(args_value)
                    .build(),
            ));
        }
    }

    fn add_instant(&mut self, instant: &protocol::ArchivedInstant) {
        let mut args = serde_json::Map::new();
        for (key, value) in instant.labels.strings.iter() {
            args.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
        for (key, value) in instant.labels.ints.iter() {
            args.insert(key.to_string(), serde_json::Value::from(value.to_native()));
        }
        for (key, value) in instant.labels.floats.iter() {
            args.insert(key.to_string(), serde_json::Value::from(value.to_native()));
        }
        for (key, value) in instant.labels.bools.iter() {
            args.insert(key.to_string(), serde_json::Value::from(*value));
        }
        let args_value = if args.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(args))
        };
        let ts = instant.timestamp.to_native();
        self.events.push(TraceEvent::Instant(
            InstantEvent::builder()
                .name(instant.name.to_string())
                .cat("instant".to_string())
                .ph(Phase::Instant)
                .ts(ts)
                .pid(instant.pid.to_native() as u32)
                .tid(instant.tid.to_native() as u32)
                .s(InstantScope::Thread)
                .maybe_args(args_value)
                .build(),
        ));
    }

    fn add_thread_name(&mut self, thread_name: &protocol::ArchivedThreadName) {
        self.events.push(TraceEvent::Metadata(
            MetadataEvent::builder()
                .ph(Phase::Metadata)
                .pid(thread_name.pid.to_native() as u32)
                .tid(thread_name.tid.to_native() as u32)
                .name(MetadataName::ThreadName)
                .args(serde_json::json!({"name": thread_name.name.to_string()}))
                .build(),
        ));
    }

    fn add_process_name(&mut self, process_name: &protocol::ArchivedProcessName) {
        self.events.push(TraceEvent::Metadata(
            MetadataEvent::builder()
                .ph(Phase::Metadata)
                .pid(process_name.pid.to_native() as u32)
                .name(MetadataName::ProcessName)
                .args(serde_json::json!({"name": process_name.name.to_string()}))
                .build(),
        ));
    }

    fn into_chrome_trace(self) -> ChromeTrace {
        ChromeTrace::builder()
            .trace_events(self.events)
            .display_time_unit("ns".to_string())
            .metadata(serde_json::json!({
                "generator": "usertrace",
                "version": "0.1.0"
            }))
            .build()
    }
}

fn parse_log_level(level: &str) -> Result<LogLevel> {
    match level.to_lowercase().as_str() {
        "error" => Ok(LogLevel::Error),
        "warn" => Ok(LogLevel::Warn),
        "info" => Ok(LogLevel::Info),
        "debug" => Ok(LogLevel::Debug),
        "trace" => Ok(LogLevel::Trace),
        _ => Err(eyre::eyre!("Invalid log level: {}", level)),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let log_level = parse_log_level(&args.log_level)?;
    let mut consumer = Consumer::new(args.buffer_size)?;
    let mut client = AgentClient::new(args.socket.clone());
    client.start(&consumer, log_level)?;

    debug!("connected to agent at {}", args.socket);
    debug!("collecting events for {} seconds...", args.duration);

    let mut converter = TraceConverter::new();

    let start_time = std::time::Instant::now();
    let duration = Duration::from_secs(args.duration);
    while start_time.elapsed() < duration {
        if let Err(e) = client.send_continue() {
            info!("failed to send keepalive: {}", e);
        }
        while let Some(record) = consumer.consume() {
            match record.as_event() {
                Ok(event) => converter.convert_event(event),
                Err(e) => info!("failed to deserialize event: {}", e),
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    client.stop()?;
    debug!("collected {} events", converter.events.len());
    let chrome_trace = converter.into_chrome_trace();
    let json = serde_json::to_string(&chrome_trace)?;
    let mut file = File::create(&args.output)?;
    file.write_all(json.as_bytes())?;
    debug!("trace written to: {}", args.output.display());
    Ok(())
}
