use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;

use rkyv::api::high::{to_bytes_in, HighSerializer};
use rkyv::rancor::{fail, Fallible};
use rkyv::ser::allocator::ArenaHandle;
use rkyv::ser::{Positional, Writer};
use rkyv::with::{AsOwned, Identity, InlineAsBox, Map, MapKV};
use rkyv::{Archive, Archived, Deserialize, Serialize};

pub const VERSION: &str = "0.1";

pub type StreamId = u16;

pub struct StreamIdAllocator {
    next_id: StreamId,
}

impl StreamIdAllocator {
    pub fn new() -> Self {
        Self { next_id: 1 }
    }

    pub fn allocate(&mut self) -> StreamId {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

impl Default for StreamIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Archive, Deserialize, Serialize, Clone)]
pub struct Labels<'a> {
    #[rkyv(with = MapKV<InlineAsBox, AsOwned>)]
    pub strings: HashMap<&'a str, Cow<'a, str>>,
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

impl<'a> InternedData<'a> {
    pub fn new() -> Self {
        InternedData {
            function_names: Vec::new(),
            frames: Vec::new(),
            callstacks: Vec::new(),
            mappings: Vec::new(),
            build_ids: Vec::new(),
            continuation: false,
        }
    }
}

impl<'a> Default for InternedData<'a> {
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
    #[rkyv(with = AsOwned)]
    pub labels: Cow<'a, Labels<'a>>,
    #[rkyv(with = Map<InlineAsBox>)]
    pub unit: Option<&'a str>,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Span<'a> {
    #[rkyv(with = InlineAsBox)]
    pub name: &'a str,
    pub span_id: u64,
    pub start_timestamp: u64,
    pub end_timestamp: u64,
    pub tid: i32,
    pub pid: i32,
    #[rkyv(with = AsOwned)]
    pub labels: Cow<'a, Labels<'a>>,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Instant<'a> {
    #[rkyv(with = InlineAsBox)]
    pub name: &'a str,
    pub timestamp: u64,
    pub tid: i32,
    pub pid: i32,
    #[rkyv(with = AsOwned)]
    pub labels: Cow<'a, Labels<'a>>,
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

#[derive(Archive, Serialize, Deserialize)]
pub struct InternedString<'a> {
    pub iid: u64,
    #[rkyv(with = AsOwned)]
    pub str: Cow<'a, str>,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Frame {
    pub iid: u64,
    pub function_name_id: u64,
    pub mapping_id: u64,
    pub rel_pc: u64,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Callstack {
    pub iid: u64,
    pub frame_ids: Vec<u64>,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Mapping {
    pub iid: u64,
    pub build_id: u64,
    pub exact_offset: u64,
    pub start_offset: u64,
    pub start: u64,
    pub end: u64,
    pub load_bias: u64,
    pub path_string_ids: Vec<u64>,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct InternedData<'a> {
    pub function_names: Vec<InternedString<'a>>,
    pub frames: Vec<Frame>,
    pub callstacks: Vec<Callstack>,
    pub mappings: Vec<Mapping>,
    pub build_ids: Vec<InternedString<'a>>,
    pub continuation: bool,
}

#[derive(Archive, Serialize, Deserialize)]
pub struct Sample {
    pub cpu: u32,
    pub pid: i32,
    pub tid: i32,
    pub timestamp: u64,
    pub callstack_iid: u64,
    pub cpu_mode: CpuMode,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[rkyv(compare(PartialEq), derive(Debug))]
pub enum CpuMode {
    Unknown,
    Kernel,
    User,
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
    InternedData(InternedData<'a>),
    Sample(Sample),
}

#[derive(Archive, Serialize, Deserialize)]
pub enum Message<'a> {
    Event(Event<'a>),
    Stream {
        stream_id: StreamId,
        event: Event<'a>,
    },
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
pub struct TracingArgs {
    pub log_filter: String,
    pub timestamp_type: TimestampType,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(compare(PartialEq))]
pub struct Args {
    pub tracing: Option<TracingArgs>,
}

impl Args {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.tracing.is_none() {
            return Err("at least one arg must be non-empty");
        }
        Ok(())
    }
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    Start {
        buffer_size: u64,
        args: Args,
    },
    Stop,
    Continue,
    Ack,
    Nack {
        error: String,
    },
    Version,
    VersionResponse {
        #[rkyv(with = InlineAsBox)]
        version: &'static str,
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

pub trait CallstackIterable {
    type Item;
    fn iid(&self) -> u64;
    fn frame_ids(&self) -> Vec<u64>;
}

pub trait FrameIterable {
    fn iid(&self) -> u64;
    fn function_name_id(&self) -> u64;
    fn mapping_id(&self) -> u64;
    fn rel_pc(&self) -> u64;
}

pub trait InternedStringIterable {
    fn iid(&self) -> u64;
    fn str_ref(&self) -> &str;
}

pub trait MappingIterable {
    fn iid(&self) -> u64;
    fn build_id(&self) -> u64;
    fn exact_offset(&self) -> u64;
    fn start_offset(&self) -> u64;
    fn start(&self) -> u64;
    fn end(&self) -> u64;
    fn load_bias(&self) -> u64;
    fn path_string_ids(&self) -> Vec<u64>;
}

pub trait InternedDataIterable<'a> {
    type Callstack: CallstackIterable + 'a;
    type Frame: FrameIterable + 'a;
    type FunctionName: InternedStringIterable + 'a;
    type Mapping: MappingIterable + 'a;
    type BuildId: InternedStringIterable + 'a;

    type CallstackIter: Iterator<Item = &'a Self::Callstack> + Clone;
    type FrameIter: Iterator<Item = &'a Self::Frame> + Clone;
    type FunctionNameIter: Iterator<Item = &'a Self::FunctionName> + Clone;
    type MappingIter: Iterator<Item = &'a Self::Mapping> + Clone;
    type BuildIdIter: Iterator<Item = &'a Self::BuildId> + Clone;

    fn callstacks(&'a self) -> Self::CallstackIter;
    fn frames(&'a self) -> Self::FrameIter;
    fn function_names(&'a self) -> Self::FunctionNameIter;
    fn mappings(&'a self) -> Self::MappingIter;
    fn build_ids(&'a self) -> Self::BuildIdIter;
    fn continuation(&self) -> bool;
}

impl CallstackIterable for Callstack {
    type Item = u64;

    fn iid(&self) -> u64 {
        self.iid
    }

    fn frame_ids(&self) -> Vec<u64> {
        self.frame_ids.clone()
    }
}

impl FrameIterable for Frame {
    fn iid(&self) -> u64 {
        self.iid
    }

    fn function_name_id(&self) -> u64 {
        self.function_name_id
    }

    fn mapping_id(&self) -> u64 {
        self.mapping_id
    }

    fn rel_pc(&self) -> u64 {
        self.rel_pc
    }
}

impl<'a> InternedStringIterable for InternedString<'a> {
    fn iid(&self) -> u64 {
        self.iid
    }

    fn str_ref(&self) -> &str {
        self.str.as_ref()
    }
}

impl MappingIterable for Mapping {
    fn iid(&self) -> u64 {
        self.iid
    }

    fn build_id(&self) -> u64 {
        self.build_id
    }

    fn exact_offset(&self) -> u64 {
        self.exact_offset
    }

    fn start_offset(&self) -> u64 {
        self.start_offset
    }

    fn start(&self) -> u64 {
        self.start
    }

    fn end(&self) -> u64 {
        self.end
    }

    fn load_bias(&self) -> u64 {
        self.load_bias
    }

    fn path_string_ids(&self) -> Vec<u64> {
        self.path_string_ids.clone()
    }
}

impl<'a> InternedDataIterable<'a> for InternedData<'a> {
    type Callstack = Callstack;
    type Frame = Frame;
    type FunctionName = InternedString<'a>;
    type Mapping = Mapping;
    type BuildId = InternedString<'a>;

    type CallstackIter = std::slice::Iter<'a, Callstack>;
    type FrameIter = std::slice::Iter<'a, Frame>;
    type FunctionNameIter = std::slice::Iter<'a, InternedString<'a>>;
    type MappingIter = std::slice::Iter<'a, Mapping>;
    type BuildIdIter = std::slice::Iter<'a, InternedString<'a>>;

    fn callstacks(&'a self) -> Self::CallstackIter {
        self.callstacks.iter()
    }

    fn frames(&'a self) -> Self::FrameIter {
        self.frames.iter()
    }

    fn function_names(&'a self) -> Self::FunctionNameIter {
        self.function_names.iter()
    }

    fn mappings(&'a self) -> Self::MappingIter {
        self.mappings.iter()
    }

    fn build_ids(&'a self) -> Self::BuildIdIter {
        self.build_ids.iter()
    }

    fn continuation(&self) -> bool {
        self.continuation
    }
}

impl CallstackIterable for ArchivedCallstack {
    type Item = Archived<u64>;

    fn iid(&self) -> u64 {
        self.iid.to_native()
    }

    fn frame_ids(&self) -> Vec<u64> {
        self.frame_ids.iter().map(|id| id.to_native()).collect()
    }
}

impl FrameIterable for ArchivedFrame {
    fn iid(&self) -> u64 {
        self.iid.to_native()
    }

    fn function_name_id(&self) -> u64 {
        self.function_name_id.to_native()
    }

    fn mapping_id(&self) -> u64 {
        self.mapping_id.to_native()
    }

    fn rel_pc(&self) -> u64 {
        self.rel_pc.to_native()
    }
}

impl<'a> InternedStringIterable for ArchivedInternedString<'a> {
    fn iid(&self) -> u64 {
        self.iid.to_native()
    }

    fn str_ref(&self) -> &str {
        self.str.as_ref()
    }
}

type ArchivedVecIter<'a, T> = std::slice::Iter<'a, T>;

impl MappingIterable for ArchivedMapping {
    fn iid(&self) -> u64 {
        self.iid.to_native()
    }

    fn build_id(&self) -> u64 {
        self.build_id.to_native()
    }

    fn exact_offset(&self) -> u64 {
        self.exact_offset.to_native()
    }

    fn start_offset(&self) -> u64 {
        self.start_offset.to_native()
    }

    fn start(&self) -> u64 {
        self.start.to_native()
    }

    fn end(&self) -> u64 {
        self.end.to_native()
    }

    fn load_bias(&self) -> u64 {
        self.load_bias.to_native()
    }

    fn path_string_ids(&self) -> Vec<u64> {
        self.path_string_ids
            .iter()
            .map(|id| id.to_native())
            .collect()
    }
}

impl<'a> InternedDataIterable<'a> for ArchivedInternedData<'a> {
    type Callstack = ArchivedCallstack;
    type Frame = ArchivedFrame;
    type FunctionName = ArchivedInternedString<'a>;
    type Mapping = ArchivedMapping;
    type BuildId = ArchivedInternedString<'a>;

    type CallstackIter = ArchivedVecIter<'a, ArchivedCallstack>;
    type FrameIter = ArchivedVecIter<'a, ArchivedFrame>;
    type FunctionNameIter = ArchivedVecIter<'a, ArchivedInternedString<'a>>;
    type MappingIter = ArchivedVecIter<'a, ArchivedMapping>;
    type BuildIdIter = ArchivedVecIter<'a, ArchivedInternedString<'a>>;

    fn callstacks(&'a self) -> Self::CallstackIter {
        self.callstacks.as_slice().iter()
    }

    fn frames(&'a self) -> Self::FrameIter {
        self.frames.as_slice().iter()
    }

    fn function_names(&'a self) -> Self::FunctionNameIter {
        self.function_names.as_slice().iter()
    }

    fn mappings(&'a self) -> Self::MappingIter {
        self.mappings.as_slice().iter()
    }

    fn build_ids(&'a self) -> Self::BuildIdIter {
        self.build_ids.as_slice().iter()
    }

    fn continuation(&self) -> bool {
        self.continuation
    }
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
            labels: Cow::Owned(Labels::default()),
            pid: 123,
            tid: 456,
            timestamp: 1000,
            unit: None,
        }
    }

    #[fixture]
    fn sample_span() -> Span<'static> {
        Span {
            name: "test_span",
            span_id: 42,
            start_timestamp: 1000,
            end_timestamp: 2000,
            tid: 1,
            pid: 2,
            labels: Cow::Owned(Labels::default()),
        }
    }

    #[fixture]
    fn sample_instant() -> Instant<'static> {
        Instant {
            name: "test_instant",
            timestamp: 2000,
            tid: 3,
            pid: 4,
            labels: Cow::Owned(Labels::default()),
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
        assert_eq!(archived.start_timestamp, sample_span.start_timestamp);
        assert_eq!(archived.end_timestamp, sample_span.end_timestamp);
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
        sample_counter: Counter<'static>,
        sample_span: Span<'static>,
        sample_instant: Instant<'static>,
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
                    assert_eq!(span.start_timestamp, arch_span.start_timestamp.to_native());
                    assert_eq!(span.end_timestamp, arch_span.end_timestamp.to_native());
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
    fn test_interned_data_serialization() {
        let mut interned_data = InternedData::new();

        interned_data.function_names.push(InternedString {
            iid: 1,
            str: Cow::Borrowed("main"),
        });
        interned_data.function_names.push(InternedString {
            iid: 2,
            str: Cow::Borrowed("process_data"),
        });

        interned_data.frames.push(Frame {
            iid: 1,
            function_name_id: 1,
            mapping_id: 1,
            rel_pc: 0x1000,
        });
        interned_data.frames.push(Frame {
            iid: 2,
            function_name_id: 2,
            mapping_id: 1,
            rel_pc: 0x2000,
        });

        interned_data.callstacks.push(Callstack {
            iid: 1,
            frame_ids: vec![1, 2],
        });

        interned_data.mappings.push(Mapping {
            iid: 1,
            build_id: 1,
            exact_offset: 0,
            start_offset: 0,
            start: 0x400000,
            end: 0x500000,
            load_bias: 0,
            path_string_ids: vec![],
        });

        interned_data.build_ids.push(InternedString {
            iid: 1,
            str: Cow::Borrowed("abcdef123456"),
        });

        let event = Event::InternedData(interned_data);
        let buf = to_bytes_in::<_, Error>(&event, Vec::new())
            .expect("interned data serialization failed");
        let archived = rkyv::access::<ArchivedEvent, rkyv::rancor::Error>(&buf)
            .expect("failed to access archived event");

        match archived {
            ArchivedEvent::InternedData(data) => {
                assert_eq!(data.function_names.len(), 2);
                assert_eq!(data.function_names[0].iid, 1);
                assert_eq!(data.function_names[0].str.as_ref().as_bytes(), b"main");
                assert_eq!(data.function_names[1].iid, 2);
                assert_eq!(
                    data.function_names[1].str.as_ref().as_bytes(),
                    b"process_data"
                );

                assert_eq!(data.frames.len(), 2);
                assert_eq!(data.frames[0].iid, 1);
                assert_eq!(data.frames[0].function_name_id, 1);
                assert_eq!(data.frames[0].rel_pc, 0x1000);

                assert_eq!(data.callstacks.len(), 1);
                assert_eq!(data.callstacks[0].iid, 1);
                assert_eq!(data.callstacks[0].frame_ids.len(), 2);
            }
            _ => panic!("expected interned data event"),
        }
    }

    #[test]
    fn test_sample_serialization() {
        let sample = Sample {
            cpu: 0,
            pid: 1234,
            tid: 5678,
            timestamp: 1000000,
            callstack_iid: 1,
            cpu_mode: CpuMode::User,
        };

        let event = Event::Sample(sample);
        let buf = to_bytes_in::<_, Error>(&event, Vec::new()).expect("sample serialization failed");
        let archived = rkyv::access::<ArchivedEvent, rkyv::rancor::Error>(&buf)
            .expect("failed to access archived event");

        match archived {
            ArchivedEvent::Sample(s) => {
                assert_eq!(s.cpu, 0);
                assert_eq!(s.pid, 1234);
                assert_eq!(s.tid, 5678);
                assert_eq!(s.timestamp, 1000000);
                assert_eq!(s.callstack_iid, 1);
                assert_eq!(s.cpu_mode, CpuMode::User);
            }
            _ => panic!("expected sample event"),
        }
    }

    #[test]
    fn test_control_message_serialization() {
        let messages = [
            ControlMessage::Start {
                buffer_size: 1024,
                args: Args {
                    tracing: Some(TracingArgs {
                        log_filter: "info".to_string(),
                        timestamp_type: TimestampType::Monotonic,
                    }),
                },
            },
            ControlMessage::Stop,
            ControlMessage::Continue,
            ControlMessage::Ack,
            ControlMessage::Nack {
                error: "connection failed".to_owned(),
            },
            ControlMessage::Version,
            ControlMessage::VersionResponse { version: VERSION },
        ];

        for msg in messages {
            let buf = to_bytes_in::<_, Error>(&msg, Vec::new())
                .expect("control message serialization failed");
            let archived = rkyv::access::<ArchivedControlMessage, rkyv::rancor::Error>(&buf)
                .expect("failed to access archived control message");

            match (&msg, archived) {
                (
                    ControlMessage::Start { buffer_size, args },
                    ArchivedControlMessage::Start {
                        buffer_size: arch_size,
                        args: arch_args,
                    },
                ) => {
                    assert_eq!(*buffer_size, arch_size.to_native());
                    match (&args.tracing, &arch_args.tracing) {
                        (
                            Some(tracing_args),
                            rkyv::option::ArchivedOption::Some(arch_tracing_args),
                        ) => {
                            assert_eq!(
                                &tracing_args.log_filter,
                                std::str::from_utf8(arch_tracing_args.log_filter.as_bytes())
                                    .unwrap()
                            );
                            assert_eq!(
                                tracing_args.timestamp_type,
                                arch_tracing_args.timestamp_type
                            );
                        }
                        _ => panic!("mismatched tracing args"),
                    }
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
                (ControlMessage::Version, ArchivedControlMessage::Version) => {}
                (
                    ControlMessage::VersionResponse { version },
                    ArchivedControlMessage::VersionResponse {
                        version: arch_version,
                    },
                ) => {
                    assert_eq!(version.as_bytes(), arch_version.as_bytes());
                }
                _ => panic!("mismatched control message variants"),
            }
        }
    }
}
