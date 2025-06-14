use agent::Agent;
use protocol::{Event, Instant, Labels, Span, SpanEvent};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{Id, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

thread_local! {
    static THREAD_ID: i32 = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
}

pub(crate) struct StoredLabels {
    pub labels: Labels<'static>,
    pub strings_storage: HashMap<&'static str, String>,
}

fn get_thread_id() -> i32 {
    THREAD_ID.with(|&tid| tid)
}

fn get_process_id() -> i32 {
    std::process::id() as i32
}

fn get_timestamp() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

pub struct ManytraceLayer {
    agent: Arc<Agent>,
}

impl ManytraceLayer {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }

    fn metadata_to_labels<'b>(&self, metadata: &'b Metadata<'b>) -> Labels<'b> {
        let mut strings = HashMap::new();
        strings.insert("target", metadata.target());
        strings.insert(
            "level",
            match *metadata.level() {
                Level::ERROR => "error",
                Level::WARN => "warn",
                Level::INFO => "info",
                Level::DEBUG => "debug",
                Level::TRACE => "trace",
            },
        );
        if let Some(module_path) = metadata.module_path() {
            strings.insert("module_path", module_path);
        }
        if let Some(file) = metadata.file() {
            strings.insert("file", file);
        }
        let mut ints = HashMap::new();
        if let Some(line) = metadata.line() {
            ints.insert("line", line as i64);
        }
        Labels {
            strings,
            ints,
            bools: HashMap::new(),
            floats: HashMap::new(),
        }
    }
}

impl<S> Layer<S> for ManytraceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if !self.agent.enabled() {
            return;
        }
        let metadata = attrs.metadata();
        let labels = self.metadata_to_labels(metadata);
        let labels_static = unsafe { std::mem::transmute::<Labels<'_>, Labels<'static>>(labels) };
        let strings_storage = HashMap::new();
        let mut stored_labels = StoredLabels {
            labels: labels_static,
            strings_storage,
        };
        attrs.record(&mut stored_labels);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(stored_labels);
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(stored_labels) = span.extensions().get::<StoredLabels>() {
                let metadata = span.metadata();
                let span_id = id.into_u64();
                let span_event = Span {
                    name: metadata.name(),
                    span_id,
                    event: SpanEvent::Start,
                    timestamp: get_timestamp(),
                    tid: get_thread_id(),
                    pid: get_process_id(),
                    labels: stored_labels.labels.clone(),
                };
                let _ = self.agent.submit(&Event::Span(span_event));
            }
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(stored_labels) = span.extensions().get::<StoredLabels>() {
                let metadata = span.metadata();
                let span_id = id.into_u64();
                let span_event = Span {
                    name: metadata.name(),
                    span_id,
                    event: SpanEvent::Stop,
                    timestamp: get_timestamp(),
                    tid: get_thread_id(),
                    pid: get_process_id(),
                    labels: stored_labels.labels.clone(),
                };
                let _ = self.agent.submit(&Event::Span(span_event));
            }
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            if let Some(stored_labels) = span.extensions().get::<StoredLabels>() {
                let metadata = span.metadata();
                let span_id = id.into_u64();
                let span_event = Span {
                    name: metadata.name(),
                    span_id,
                    event: SpanEvent::End,
                    timestamp: get_timestamp(),
                    tid: get_thread_id(),
                    pid: get_process_id(),
                    labels: stored_labels.labels.clone(),
                };
                let _ = self.agent.submit(&Event::Span(span_event));
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if !self.agent.enabled() {
            return;
        }
        let metadata = event.metadata();
        let labels = self.metadata_to_labels(metadata);
        let labels_static = unsafe { std::mem::transmute::<Labels<'_>, Labels<'static>>(labels) };
        let mut stored_labels = StoredLabels {
            labels: labels_static,
            strings_storage: HashMap::new(),
        };
        event.record(&mut stored_labels);
        let instant = Instant {
            name: metadata.name(),
            timestamp: get_timestamp(),
            tid: get_thread_id(),
            pid: get_process_id(),
            labels: stored_labels.labels,
        };
        let _ = self.agent.submit(&Event::Instant(instant));
    }
}
