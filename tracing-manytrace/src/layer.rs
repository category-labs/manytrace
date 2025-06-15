use agent::Agent;
use protocol::{Event, Instant, Labels, ProcessName, Span, SpanEvent, ThreadName};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{Id, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use std::cell::Cell;
use std::sync::OnceLock;

thread_local! {
    static THREAD_ID: i32 = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
    static THREAD_CLIENT_ID: Cell<Option<u64>> = const { Cell::new(None) };
}

static PROCESS_ID: OnceLock<i32> = OnceLock::new();
static PROCESS_CLIENT_ID: std::sync::Mutex<Option<u64>> = std::sync::Mutex::new(None);

pub(crate) struct StoredLabels {
    pub labels: Labels<'static>,
    pub strings_storage: HashMap<&'static str, String>,
}

fn get_thread_id() -> i32 {
    THREAD_ID.with(|&tid| tid)
}

fn get_process_id() -> i32 {
    *PROCESS_ID.get_or_init(|| std::process::id() as i32)
}

fn get_timestamp(agent: &Agent) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(agent.clock_id(), &mut ts);
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

    fn maybe_emit_thread_process_names(&self) {
        let current_client_id = self.agent.client_id();
        if current_client_id.is_none() {
            return;
        }

        let pid = get_process_id();

        if let Ok(mut stored_client_id) = PROCESS_CLIENT_ID.lock() {
            if *stored_client_id != current_client_id {
                *stored_client_id = current_client_id;

                let process_name = std::env::current_exe()
                    .ok()
                    .and_then(|path| path.file_name().map(|s| s.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| format!("process-{}", pid));

                let event = Event::ProcessName(ProcessName {
                    name: &process_name,
                    pid,
                });

                let _ = self.agent.submit(&event);
            }
        }

        THREAD_CLIENT_ID.with(|stored| {
            if stored.get() != current_client_id {
                stored.set(current_client_id);

                let tid = get_thread_id();
                let thread_name = std::thread::current()
                    .name()
                    .map(|s| s.to_owned())
                    .unwrap_or_else(|| format!("thread-{}", tid));

                let event = Event::ThreadName(ThreadName {
                    name: &thread_name,
                    tid,
                    pid,
                });

                let _ = self.agent.submit(&event);
            }
        });
    }

    fn create_labels(&self, _metadata: &'static Metadata<'static>) -> Labels<'static> {
        Labels {
            strings: HashMap::new(),
            ints: HashMap::new(),
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
        let labels = self.create_labels(metadata);
        let strings_storage = HashMap::new();
        let mut stored_labels = StoredLabels {
            labels,
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
                self.maybe_emit_thread_process_names();

                let metadata = span.metadata();
                let span_id = id.into_u64();
                let span_event = Span {
                    name: metadata.name(),
                    span_id,
                    event: SpanEvent::Start,
                    timestamp: get_timestamp(&self.agent),
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
                    timestamp: get_timestamp(&self.agent),
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
                    timestamp: get_timestamp(&self.agent),
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

        self.maybe_emit_thread_process_names();

        let metadata = event.metadata();
        let labels = self.create_labels(metadata);
        let mut stored_labels = StoredLabels {
            labels,
            strings_storage: HashMap::new(),
        };
        event.record(&mut stored_labels);
        let instant = Instant {
            name: metadata.name(),
            timestamp: get_timestamp(&self.agent),
            tid: get_thread_id(),
            pid: get_process_id(),
            labels: stored_labels.labels.clone(),
        };
        let _ = self.agent.submit(&Event::Instant(instant));
    }
}
