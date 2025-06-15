use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;

use rkyv::api::high::{to_bytes_in, HighSerializer};
use rkyv::rancor::{fail, Fallible};
use rkyv::ser::allocator::ArenaHandle;
use rkyv::ser::{Positional, Writer};
use rkyv::with::{Identity, InlineAsBox, MapKV};
use rkyv::{Archive, Deserialize, Serialize};

#[derive(Archive, Deserialize, Serialize, Clone)]
pub struct Labels<'a> {
    #[rkyv(with = MapKV<InlineAsBox, InlineAsBox>)]
    pub strings: HashMap<&'a str, &'a str>,
    #[rkyv(with = MapKV<InlineAsBox, Identity>)]
    pub ints: HashMap<&'a str, i64>,
    #[rkyv(with = MapKV<InlineAsBox, Identity>)]
    pub bools: HashMap<&'a str, bool>,
    #[rkyv(with = MapKV<InlineAsBox, Identity>)]
    pub floats: HashMap<&'a str, f64>,
}

impl<'a> Labels<'a> {
    pub fn new() -> Self {
        Labels {
            strings: HashMap::new(),
            ints: HashMap::new(),
            bools: HashMap::new(),
            floats: HashMap::new(),
        }
    }
}

impl<'a> Default for Labels<'a> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Counter<'a> {
    #[rkyv(with = InlineAsBox)]
    pub name: &'a str,
    pub value: f64,
    pub timestamp: u64,
    pub tid: i32,
    pub pid: i32,
    pub labels: Labels<'a>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum SpanEvent {
    Start,
    Stop,
    End,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Span<'a> {
    #[rkyv(with = InlineAsBox)]
    pub name: &'a str,
    pub span_id: u64,
    pub event: SpanEvent,
    pub timestamp: u64,
    pub tid: i32,
    pub pid: i32,
    pub labels: Labels<'a>,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Instant<'a> {
    #[rkyv(with = InlineAsBox)]
    pub name: &'a str,
    pub timestamp: u64,
    pub tid: i32,
    pub pid: i32,
    pub labels: Labels<'a>,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct ThreadName<'a> {
    #[rkyv(with = InlineAsBox)]
    pub name: &'a str,
    pub tid: i32,
    pub pid: i32,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct ProcessName<'a> {
    #[rkyv(with = InlineAsBox)]
    pub name: &'a str,
    pub pid: i32,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Archive, Serialize, Deserialize)]
pub enum Event<'a> {
    Counter(Counter<'a>),
    Span(Span<'a>),
    Instant(Instant<'a>),
    ThreadName(ThreadName<'a>),
    ProcessName(ProcessName<'a>),
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum TimestampType {
    Monotonic,
    Boottime,
    Realtime,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(compare(PartialEq))]
pub enum ControlMessage<'a> {
    Start {
        buffer_size: u64,
        log_level: LogLevel,
        timestamp_type: TimestampType,
    },
    Stop,
    Continue,
    Ack,
    Nack {
        #[rkyv(with = InlineAsBox)]
        error: &'a str,
    },
}

pub struct CountingWriter {
    write_count: usize,
    total_bytes: usize,
}

impl CountingWriter {
    pub fn new() -> Self {
        Self {
            write_count: 0,
            total_bytes: 0,
        }
    }

    pub fn write_count(&self) -> usize {
        self.write_count
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

impl Default for CountingWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl Fallible for CountingWriter {
    type Error = rkyv::rancor::Error;
}

impl Positional for CountingWriter {
    fn pos(&self) -> usize {
        self.total_bytes
    }
}

impl Writer<rkyv::rancor::Error> for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> Result<(), rkyv::rancor::Error> {
        self.write_count += 1;
        self.total_bytes += bytes.len();
        Ok(())
    }
}

pub struct SliceWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> SliceWriter<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn written(&self) -> usize {
        self.pos
    }
}

impl<'a> Fallible for SliceWriter<'a> {
    type Error = rkyv::rancor::Error;
}

impl<'a> Positional for SliceWriter<'a> {
    fn pos(&self) -> usize {
        self.pos
    }
}

#[derive(Debug)]
pub struct OutOfSpaceError;

impl Display for OutOfSpaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not enough space in the buffer",)
    }
}

impl Error for OutOfSpaceError {}

impl<'a> Writer<rkyv::rancor::Error> for SliceWriter<'a> {
    fn write(&mut self, bytes: &[u8]) -> Result<(), rkyv::rancor::Error> {
        if self.pos + bytes.len() > self.buf.len() {
            fail!(OutOfSpaceError);
        }
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
        Ok(())
    }
}

/// The caller must ensure that `buf` has sufficient capacity to hold the serialized data.
/// Use `compute_length()` first to determine the required buffer size.
///
/// # Safety Conditions
/// - `buf.len()` must be >= the size returned by `compute_length(value)`
/// - `buf` must be properly aligned for the serialized data
/// - The buffer must remain valid for the duration of the serialization
///
/// # Errors
/// Returns `rkyv::rancor::Error` if the buffer is too small or serialization fails.
pub fn serialize_to_buf<'b, T>(value: &T, buf: &'b mut [u8]) -> Result<(), rkyv::rancor::Error>
where
    T: for<'a> Serialize<HighSerializer<SliceWriter<'b>, ArenaHandle<'a>, rkyv::rancor::Error>>,
{
    let writer = SliceWriter::new(buf);
    let _ = to_bytes_in(value, writer)?;
    Ok(())
}

pub fn compute_length<'a, T>(value: &'a T) -> Result<usize, rkyv::rancor::Error>
where
    T: for<'b> Serialize<HighSerializer<CountingWriter, ArenaHandle<'b>, rkyv::rancor::Error>>,
{
    let writer = CountingWriter::new();
    let w = to_bytes_in(value, writer)?;
    Ok(w.total_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rkyv::rancor::Error;
    use rstest::*;

    #[fixture]
    fn sample_counter() -> Counter<'static> {
        Counter {
            name: "test_counter",
            value: 42.0,
            labels: Labels::default(),
            pid: 123,
            tid: 456,
            timestamp: 1000,
        }
    }

    #[fixture]
    fn sample_span() -> Span<'static> {
        Span {
            name: "test_span",
            span_id: 42,
            event: SpanEvent::Start,
            timestamp: 1000,
            tid: 1,
            pid: 2,
            labels: Labels::default(),
        }
    }

    #[fixture]
    fn sample_instant() -> Instant<'static> {
        Instant {
            name: "test_instant",
            timestamp: 2000,
            tid: 3,
            pid: 4,
            labels: Labels::default(),
        }
    }

    #[rstest]
    fn test_counting_writer(sample_counter: Counter) {
        let mut counting_writer = CountingWriter::new();
        to_bytes_in(&sample_counter, &mut counting_writer).expect("counting failed");

        let expected_size = counting_writer.total_bytes();
        let write_count = counting_writer.write_count();

        assert!(expected_size > 0, "Expected size should be greater than 0");
        assert!(write_count > 0, "Write count should be greater than 0");

        let buf = to_bytes_in::<_, Error>(&sample_counter, Vec::new()).expect("serialized");
        assert_eq!(
            buf.len(),
            expected_size,
            "Buffer size should match counted size"
        );

        let archived = rkyv::access::<ArchivedCounter, rkyv::rancor::Error>(&buf).unwrap();
        assert_eq!(archived.name.as_bytes(), sample_counter.name.as_bytes());
        assert_eq!(archived.value, 42.0);

        println!(
            "Serialized size: {}, Write operations: {}",
            expected_size, write_count
        );
    }

    #[rstest]
    fn test_unsafe_serialize_to_buf(sample_counter: Counter) {
        let required_size = compute_length(&sample_counter).expect("compute length failed");

        let mut buf = vec![0u8; required_size];

        serialize_to_buf(&sample_counter, &mut buf).expect("serialization failed");

        let archived = rkyv::access::<ArchivedCounter, rkyv::rancor::Error>(&buf).unwrap();
        assert_eq!(archived.name.as_bytes(), sample_counter.name.as_bytes());
        assert_eq!(archived.value, sample_counter.value);
    }

    #[rstest]
    fn test_compute_length(sample_span: Span) {
        let computed_size = compute_length(&sample_span).expect("compute length failed");
        let serialized =
            to_bytes_in::<_, Error>(&sample_span, vec![]).expect("serialization failed");

        assert_eq!(
            computed_size,
            serialized.len(),
            "Computed size should match actual serialized size"
        );
        assert!(computed_size > 0, "Computed size should be greater than 0");
    }

    #[rstest]
    fn test_span_serialization(sample_span: Span) {
        let buf =
            to_bytes_in::<_, Error>(&sample_span, Vec::new()).expect("span serialization failed");
        let archived = rkyv::access::<ArchivedSpan, rkyv::rancor::Error>(&buf).unwrap();

        assert_eq!(archived.name.as_bytes(), sample_span.name.as_bytes());
        assert_eq!(archived.span_id, sample_span.span_id);
        assert_eq!(archived.event, sample_span.event);
        assert_eq!(archived.timestamp, sample_span.timestamp);
    }

    #[rstest]
    fn test_instant_serialization(sample_instant: Instant) {
        let buf = to_bytes_in::<_, Error>(&sample_instant, Vec::new())
            .expect("instant serialization failed");
        let archived = rkyv::access::<ArchivedInstant, rkyv::rancor::Error>(&buf).unwrap();

        assert_eq!(archived.name.as_bytes(), sample_instant.name.as_bytes());
        assert_eq!(archived.timestamp, sample_instant.timestamp);
        assert_eq!(archived.tid, sample_instant.tid);
        assert_eq!(archived.pid, sample_instant.pid);
    }

    #[rstest]
    fn test_serialize_to_buf_error_handling(sample_counter: Counter) {
        let required_size = compute_length(&sample_counter).expect("compute length failed");
        let mut small_buf = vec![0u8; required_size - 1];

        let result = serialize_to_buf(&sample_counter, &mut small_buf);

        assert!(
            result.is_err(),
            "Should return error for insufficient buffer"
        );
    }

    #[rstest]
    fn test_event_serialization(
        sample_counter: Counter,
        sample_span: Span,
        sample_instant: Instant,
    ) {
        let events = [
            Event::Counter(sample_counter),
            Event::Span(sample_span),
            Event::Instant(sample_instant),
        ];

        for event in events {
            let buf =
                to_bytes_in::<_, Error>(&event, Vec::new()).expect("event serialization failed");
            let archived = rkyv::access::<ArchivedEvent, rkyv::rancor::Error>(&buf)
                .expect("failed to access archived event");

            match (&event, archived) {
                (Event::Counter(counter), ArchivedEvent::Counter(arch_counter)) => {
                    assert_eq!(counter.name.as_bytes(), arch_counter.name.as_bytes());
                    assert_eq!(counter.value, arch_counter.value.to_native());
                }
                (Event::Span(span), ArchivedEvent::Span(arch_span)) => {
                    assert_eq!(span.name.as_bytes(), arch_span.name.as_bytes());
                    assert_eq!(span.span_id, arch_span.span_id.to_native());
                    assert_eq!(span.event, arch_span.event);
                }
                (Event::Instant(instant), ArchivedEvent::Instant(arch_instant)) => {
                    assert_eq!(instant.name.as_bytes(), arch_instant.name.as_bytes());
                    assert_eq!(instant.timestamp, arch_instant.timestamp.to_native());
                }
                _ => panic!("mismatched event variants"),
            }
        }
    }

    #[test]
    fn test_thread_process_name_serialization() {
        let thread_name = ThreadName {
            name: "worker-thread-1",
            tid: 12345,
            pid: 67890,
        };

        let process_name = ProcessName {
            name: "my-process",
            pid: 67890,
        };

        let thread_event = Event::ThreadName(thread_name);
        let process_event = Event::ProcessName(process_name);

        for event in [thread_event, process_event] {
            let buf =
                to_bytes_in::<_, Error>(&event, Vec::new()).expect("event serialization failed");
            let archived = rkyv::access::<ArchivedEvent, rkyv::rancor::Error>(&buf)
                .expect("failed to access archived event");

            match (&event, archived) {
                (Event::ThreadName(thread), ArchivedEvent::ThreadName(arch_thread)) => {
                    assert_eq!(thread.name.as_bytes(), arch_thread.name.as_bytes());
                    assert_eq!(thread.tid, arch_thread.tid.to_native());
                    assert_eq!(thread.pid, arch_thread.pid.to_native());
                }
                (Event::ProcessName(process), ArchivedEvent::ProcessName(arch_process)) => {
                    assert_eq!(process.name.as_bytes(), arch_process.name.as_bytes());
                    assert_eq!(process.pid, arch_process.pid.to_native());
                }
                _ => panic!("mismatched event variants"),
            }
        }
    }

    #[test]
    fn test_control_message_serialization() {
        let messages = [
            ControlMessage::Start {
                buffer_size: 1024,
                log_level: LogLevel::Info,
                timestamp_type: TimestampType::Monotonic,
            },
            ControlMessage::Stop,
            ControlMessage::Continue,
            ControlMessage::Ack,
            ControlMessage::Nack {
                error: "connection failed",
            },
        ];

        for msg in messages {
            let buf = to_bytes_in::<_, Error>(&msg, Vec::new())
                .expect("control message serialization failed");
            let archived = rkyv::access::<ArchivedControlMessage, rkyv::rancor::Error>(&buf)
                .expect("failed to access archived control message");

            match (&msg, archived) {
                (
                    ControlMessage::Start {
                        buffer_size,
                        log_level,
                        timestamp_type,
                    },
                    ArchivedControlMessage::Start {
                        buffer_size: arch_size,
                        log_level: arch_level,
                        timestamp_type: arch_timestamp_type,
                    },
                ) => {
                    assert_eq!(*buffer_size, arch_size.to_native());
                    assert_eq!(*log_level, *arch_level);
                    assert_eq!(*timestamp_type, *arch_timestamp_type);
                }
                (ControlMessage::Stop, ArchivedControlMessage::Stop) => {}
                (ControlMessage::Continue, ArchivedControlMessage::Continue) => {}
                (ControlMessage::Ack, ArchivedControlMessage::Ack) => {}
                (
                    ControlMessage::Nack { error },
                    ArchivedControlMessage::Nack { error: arch_error },
                ) => {
                    assert_eq!(error.as_bytes(), arch_error.as_bytes());
                }
                _ => panic!("mismatched control message variants"),
            }
        }
    }
}
