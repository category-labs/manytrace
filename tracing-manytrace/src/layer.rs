use agent::Agent;
use protocol::{Event, Instant, Labels, Span};
use std::borrow::Cow;
use std::sync::Arc;
use thread_local::ThreadLocal;
use tracing::{Id, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

pub(crate) struct SpanData {
    pub labels: Labels<'static>,
    pub start_timestamp: u64,
}

fn get_timestamp(clock_id: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(clock_id, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

pub struct ManytraceLayer {
    agent: Arc<Agent>,
    thread_ids: Arc<ThreadLocal<std::cell::Cell<i32>>>,
    process_id: i32,
}

impl ManytraceLayer {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self {
            agent,
            thread_ids: Arc::new(ThreadLocal::new()),
            process_id: std::process::id() as i32,
        }
    }

    fn get_thread_id(&self) -> i32 {
        let thread_id_cell = self
            .thread_ids
            .get_or(|| std::cell::Cell::new(unsafe { libc::syscall(libc::SYS_gettid) as i32 }));
        thread_id_cell.get()
    }

    fn get_process_id(&self) -> i32 {
        self.process_id
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
        let labels = Labels::new();
        let mut span_data = SpanData {
            labels,
            start_timestamp: 0,
        };
        attrs.record(&mut span_data);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(span_data);
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(span_data) = span.extensions_mut().get_mut::<SpanData>() {
                span_data.start_timestamp = get_timestamp(self.agent.clock_id());
            }
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            if let Some(span_data) = span.extensions().get::<SpanData>() {
                let metadata = span.metadata();
                let span_id = id.into_u64();
                let span_event = Span {
                    name: metadata.name(),
                    span_id,
                    start_timestamp: span_data.start_timestamp,
                    end_timestamp: get_timestamp(self.agent.clock_id()),
                    tid: self.get_thread_id(),
                    pid: self.get_process_id(),
                    labels: Cow::Borrowed(&span_data.labels),
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
        let labels = Labels::new();
        let mut span_data = SpanData {
            labels,
            start_timestamp: 0,
        };
        event.record(&mut span_data);
        let timestamp = get_timestamp(self.agent.clock_id());
        let instant_event = Instant {
            name: metadata.name(),
            timestamp,
            tid: self.get_thread_id(),
            pid: self.get_process_id(),
            labels: Cow::Borrowed(&span_data.labels),
        };
        let _ = self.agent.submit(&Event::Instant(instant_event));
    }
}
