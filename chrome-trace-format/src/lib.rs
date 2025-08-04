// Copyright (C) 2025 Category Labs, Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! # Chrome Trace Format
//!
//! This crate provides Rust types for the Chrome Trace Event Format, which is the trace data
//! representation processed by the Chrome Trace Viewer (chrome://tracing).
//!
//! The Chrome Trace Event Format is a JSON-based format for recording performance traces.
//! It supports various event types for tracking durations, async operations, counters,
//! memory dumps, and more.
//!
//! ## Format Overview
//!
//! Traces can be provided in two formats:
//! - **JSON Array Format**: A simple array of trace events
//! - **JSON Object Format**: An object containing trace events and metadata
//!
//! ## Event Types
//!
//! The format supports multiple event types, each with a specific phase:
//! - **Duration Events** (B/E): Mark the beginning and end of operations
//! - **Complete Events** (X): Combine begin/end into a single event with duration
//! - **Instant Events** (i/I): Mark points in time with no duration
//! - **Counter Events** (C): Track values over time
//! - **Async Events** (b/n/e): Track asynchronous operations across threads
//! - **Flow Events** (s/t/f): Show relationships between events across threads
//! - **Object Events** (N/O/D): Track object lifecycle and snapshots
//! - **Metadata Events** (M): Provide process/thread names and other metadata
//! - **Memory Dump Events** (V/v): Record memory usage information
//! - **Mark Events** (R): Navigation timing marks
//! - **Clock Sync Events** (c): Synchronize clocks across processes
//! - **Context Events** ((/))): Group events into contexts
//! - **Linked ID Events** (=): Link different IDs together
//!
//! ## Timestamps
//!
//! All timestamps are in microseconds by default. The `display_time_unit` field
//! in the JSON object format can specify "ms" or "ns" for display purposes.

use bon::Builder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The top-level Chrome trace format structure.
///
/// The Chrome Trace Format is the trace data representation that is processed by the
/// Chrome Trace Viewer (chrome://tracing). It supports two main formats:
///
/// 1. **JSON Array Format**: A simple array of trace events
/// 2. **JSON Object Format**: An object containing trace events and metadata
///
/// The JSON Object Format provides more flexibility with additional properties like
/// display time units, system trace data, stack frames, and custom metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct ChromeTrace {
    /// The array of trace events. This is the primary data in a trace.
    ///
    /// Events do not need to be in timestamp-sorted order. The trace viewer will
    /// automatically sort and organize events for display.
    #[serde(rename = "traceEvents", skip_serializing_if = "Option::is_none")]
    pub trace_events: Option<Vec<TraceEvent>>,

    /// Specifies the unit for displaying timestamps ("ms" or "ns").
    /// Defaults to "ms" if not specified.
    #[serde(rename = "displayTimeUnit", skip_serializing_if = "Option::is_none")]
    pub display_time_unit: Option<String>,

    /// Linux ftrace or Windows ETW trace data.
    ///
    /// For Linux ftrace data, this string must start with "# tracer:" and adhere to
    /// the Linux ftrace format. This allows integration of kernel-level trace data
    /// with application-level Chrome trace events.
    #[serde(rename = "systemTraceEvents", skip_serializing_if = "Option::is_none")]
    pub system_trace_events: Option<String>,

    /// Additional metadata for the trace that will be displayed in the Trace Viewer.
    /// This can contain any custom data and will be accessible through the Metadata button.
    #[serde(rename = "otherData", skip_serializing_if = "Option::is_none")]
    pub other_data: Option<Value>,

    /// BattOr power trace data.
    /// Used for power profiling with the BattOr power monitor device.
    #[serde(
        rename = "powerTraceAsynchronousData",
        skip_serializing_if = "Option::is_none"
    )]
    pub power_trace_asynchronous_data: Option<Value>,

    /// Dictionary of stack frames for compact representation of stack traces.
    ///
    /// Stack frames are referenced by their IDs in events using the `sf` field.
    /// This dictionary allows for efficient storage of stack traces by avoiding
    /// duplication - each unique frame is stored once and referenced by ID.
    /// Stack frames form a tree structure through parent references.
    #[serde(rename = "stackFrames", skip_serializing_if = "Option::is_none")]
    pub stack_frames: Option<StackFrames>,

    /// Array of sampling profiler data from OS-level profilers.
    ///
    /// Contains hardware-assisted or timer-driven counter sampling data.
    /// These samples augment trace event data with lower-level information
    /// from OS profilers. It's valid to have a trace with only sample data,
    /// but `traceEvents` must still be provided (can be empty array).
    #[serde(rename = "samples", skip_serializing_if = "Option::is_none")]
    pub samples: Option<Vec<Sample>>,

    /// Control flow profiling data.
    #[serde(rename = "controlFlowProfile", skip_serializing_if = "Option::is_none")]
    pub control_flow_profile: Option<Value>,

    /// General metadata for the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Represents a single trace event in the Chrome trace format.
///
/// The Chrome trace format supports various event types for tracking different aspects
/// of program execution. Each event type serves a specific purpose:
///
/// - **Duration**: Track operations with begin/end times
/// - **Complete**: Efficient representation of operations with known duration
/// - **Instant**: Mark specific points in time
/// - **Counter**: Track numeric values over time
/// - **Async**: Track asynchronous operations across threads
/// - **Flow**: Show causal relationships between events
/// - **Object**: Track object lifecycle and state
/// - **Metadata**: Provide process/thread names and configuration
/// - **Memory Dump**: Record memory usage snapshots
///
/// Events are automatically deserialized into the appropriate variant based on their phase type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TraceEvent {
    Duration(DurationEvent),
    Complete(CompleteEvent),
    Instant(InstantEvent),
    Counter(CounterEvent),
    Async(AsyncEvent),
    Flow(FlowEvent),
    Sample(SampleEvent),
    Object(ObjectEvent),
    Metadata(MetadataEvent),
    MemoryDump(MemoryDumpEvent),
    Mark(MarkEvent),
    ClockSync(ClockSyncEvent),
    Context(ContextEvent),
    LinkedId(LinkedIdEvent),
}

/// Event phase types that determine the kind of event and how it's displayed.
///
/// Each phase type has a specific single-character representation and determines
/// how the event is interpreted and visualized in the Chrome Trace Viewer.
///
/// The phase is the most important field as it determines:
/// - How the event is parsed and validated
/// - What additional fields are required or optional
/// - How the event is visualized in the trace viewer
/// - How the event relates to other events
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    /// Begin phase of a duration event.
    /// Must be paired with a corresponding DurationEnd (E) event.
    #[serde(rename = "B")]
    DurationBegin,
    /// End phase of a duration event.
    /// Must follow a corresponding DurationBegin (B) event.
    #[serde(rename = "E")]
    DurationEnd,
    /// Complete event that combines begin and end with a duration.
    /// More efficient than separate B/E events for events with known duration.
    #[serde(rename = "X")]
    Complete,
    /// Instant event with no duration.
    /// Displayed as a vertical line at a specific timestamp.
    #[serde(rename = "i")]
    Instant,
    /// Deprecated instant event phase.
    /// Use Instant (i) instead.
    #[serde(rename = "I")]
    InstantDeprecated,
    /// Counter event for tracking values over time.
    /// Displayed as a line graph in the trace viewer.
    #[serde(rename = "C")]
    Counter,
    /// Begin phase of a nestable async event.
    /// Used for asynchronous operations that can span across threads.
    #[serde(rename = "b")]
    AsyncBegin,
    /// Instant event within an async operation.
    /// Marks intermediate points in an async event sequence.
    #[serde(rename = "n")]
    AsyncStep,
    /// End phase of a nestable async event.
    /// Closes an async operation started with AsyncBegin.
    #[serde(rename = "e")]
    AsyncEnd,
    /// Start of a flow event.
    /// Creates an arrow connecting events across threads/processes.
    #[serde(rename = "s")]
    FlowBegin,
    /// Intermediate step in a flow event sequence.
    #[serde(rename = "t")]
    FlowStep,
    /// End of a flow event.
    /// Completes the flow started with FlowBegin.
    #[serde(rename = "f")]
    FlowEnd,
    /// Sample event from a sampling profiler.
    /// Deprecated in favor of global samples in the samples array.
    #[serde(rename = "P")]
    Sample,
    /// Object creation event.
    /// Marks when an object instance is created.
    #[serde(rename = "N")]
    ObjectCreated,
    /// Object destruction event.
    /// Marks when an object instance is destroyed.
    #[serde(rename = "D")]
    ObjectDestroyed,
    /// Object snapshot event.
    /// Captures the state of an object at a specific point in time.
    #[serde(rename = "O")]
    ObjectSnapshot,
    /// Metadata event for process/thread names and other information.
    #[serde(rename = "M")]
    Metadata,
    /// Global memory dump event.
    /// Contains system-wide memory information.
    #[serde(rename = "V")]
    GlobalMemoryDump,
    /// Process memory dump event.
    /// Contains memory usage information for a single process.
    #[serde(rename = "v")]
    ProcessMemoryDump,
    /// Navigation timing mark event.
    /// Created for web page lifecycle events and user-defined marks.
    #[serde(rename = "R")]
    Mark,
    /// Clock synchronization event.
    /// Used to synchronize clocks across different processes/agents.
    #[serde(rename = "c")]
    ClockSync,
    /// Enter a context.
    /// Marks the beginning of a context that groups subsequent events.
    #[serde(rename = "(")]
    ContextEnter,
    /// Leave a context.
    /// Marks the end of a context started with ContextEnter.
    #[serde(rename = ")")]
    ContextLeave,
    /// Link two IDs together.
    /// Specifies that two different IDs refer to the same logical entity.
    #[serde(rename = "=")]
    LinkedId,
}

/// Scope of an instant event, determining its visual height in the trace viewer.
///
/// The scope affects how the instant event is drawn:
/// - **Thread**: Event height is confined to a single thread lane
/// - **Process**: Event spans all threads within the process
/// - **Global**: Event spans the entire timeline from top to bottom
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstantScope {
    /// Global scope - event spans the entire timeline height.
    #[serde(rename = "g")]
    Global,
    /// Process scope - event spans all threads in a process.
    #[serde(rename = "p")]
    Process,
    /// Thread scope - event is confined to a single thread (default).
    #[serde(rename = "t")]
    Thread,
}

/// Event identifier that can be a string, number, or local ID.
///
/// IDs are used to correlate related events, especially for async and flow events.
/// By default:
/// - **Async event IDs**: Global across processes (events with same ID in different processes are grouped)
/// - **Object event IDs**: Process-local (same ID in different processes refers to different objects)
///
/// For memory addresses, use hex strings like "0x1000". The trace viewer ensures
/// that reused addresses (after free/realloc) are treated as different objects based
/// on their lifetimes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    String(String),
    Number(u64),
    Local(LocalId),
}

/// Explicitly process-local identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalId {
    /// The process-local ID value.
    pub local: String,
}

/// Explicit ID with both global and local components.
///
/// Allows explicit specification of whether an ID is process-local or global,
/// overriding the default behavior for different event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Id2 {
    /// The global component of the ID.
    pub global: String,
    /// The local component of the ID.
    pub local: String,
}

/// Duration event marking the beginning or end of an operation.
///
/// Duration events provide a way to mark a duration of work on a given thread.
/// Key requirements:
/// - Events must be properly nested (no partial overlaps)
/// - B events must come before corresponding E events
/// - Timestamps must be in increasing order within a thread
/// - Only required fields for E events are: pid, tid, ph, ts
///
/// When both B and E events have args, they are merged (E args take precedence).
/// Duration events are ideal for function tracing and performance profiling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurationEvent {
    /// Display name of the event in the trace viewer.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be DurationBegin (B) or DurationEnd (E).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Custom arguments displayed in the trace viewer.
    ///
    /// Arguments can contain any JSON data and are shown in the event details.
    /// For E events, args are merged with the corresponding B event's args
    /// (E event args take precedence for duplicate keys).
    ///
    /// Special arg properties:
    /// - `"snapshot"`: For object snapshot data
    /// - Fields with `{"id_ref": "0x1000"}`: Replaced with object snapshots
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Duration in microseconds (typically not used for B/E events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur: Option<u64>,
    /// Thread clock duration in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tdur: Option<u64>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    /// ID for binding flow events to this slice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_id: Option<String>,
    /// Indicates this slice is a flow target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_in: Option<bool>,
    /// Indicates this slice is a flow source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_out: Option<bool>,
    /// Event identifier for correlating related events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Explicit local/global identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id2: Option<Id2>,
    /// Scope for ID disambiguation.
    ///
    /// Optional string to avoid ID conflicts. Events with the same ID but
    /// different scopes are treated as separate entities. Useful when IDs
    /// might collide across different subsystems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Stack frame ID referencing an entry in the stackFrames dictionary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sf: Option<u64>,
    /// End stack frame ID (for E events) referencing the stackFrames dictionary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esf: Option<u64>,
}

/// Complete event combining begin and end with a duration.
///
/// Complete events are more efficient than separate B/E events when the duration
/// is known. They reduce trace size by about half compared to duration events.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct CompleteEvent {
    /// Display name of the event in the trace viewer.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be Complete (X).
    pub ph: Phase,
    /// Start timestamp in microseconds.
    pub ts: u64,
    /// Duration in microseconds.
    pub dur: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Thread clock duration in microseconds.
    ///
    /// Optional thread clock duration. While `dur` measures wall-clock time,
    /// `tdur` measures CPU time spent by the thread. Useful for identifying
    /// operations that are CPU-bound vs. I/O-bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tdur: Option<u64>,
    /// Thread clock timestamp in microseconds.
    ///
    /// The thread clock timestamp of the event. Thread clock measures CPU time
    /// used by the thread, excluding time spent blocked or scheduled out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    /// ID for binding flow events to this slice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_id: Option<String>,
    /// Indicates this slice is a flow target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_in: Option<bool>,
    /// Indicates this slice is a flow source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_out: Option<bool>,
    /// Event identifier for correlating related events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Explicit local/global identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id2: Option<Id2>,
    /// Scope for ID disambiguation.
    ///
    /// Optional string to avoid ID conflicts. Events with the same ID but
    /// different scopes are treated as separate entities. Useful when IDs
    /// might collide across different subsystems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Stack frame ID for the start of the event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sf: Option<u64>,
    /// Stack frame ID for the end of the event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esf: Option<u64>,
}

/// Instant event marking a point in time with no duration.
///
/// Instant events correspond to something that happens but has no duration
/// associated with it (e.g., vblank events, log messages, breakpoints).
///
/// They are displayed as vertical lines in the trace viewer, with the scope
/// determining the visual height:
/// - Thread scope: Height of a single thread lane
/// - Process scope: Spans all threads in the process  
/// - Global scope: Spans entire timeline top to bottom
///
/// Thread-scoped events support stack traces; process/global scopes do not.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct InstantEvent {
    /// Display name of the event in the trace viewer.
    ///
    /// This is how the event will be labeled in the UI. Choose descriptive names
    /// that clearly indicate what happened at this instant.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be Instant (i) or InstantDeprecated (I).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Scope determining the visual height of the instant event.
    ///
    /// Required field (defaults to thread scope if not specified).
    /// The scope affects both visualization and which events the instant
    /// is associated with in the trace analysis.
    pub s: InstantScope,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    /// ID for binding flow events to this instant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_id: Option<String>,
    /// Indicates this instant is a flow target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_in: Option<bool>,
    /// Indicates this instant is a flow source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_out: Option<bool>,
    /// Event identifier for correlating related events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Explicit local/global identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id2: Option<Id2>,
    /// Scope for ID disambiguation.
    ///
    /// Optional string to avoid ID conflicts. Events with the same ID but
    /// different scopes are treated as separate entities. Useful when IDs
    /// might collide across different subsystems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Stack frame ID (only for thread-scoped instants).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sf: Option<u64>,
    /// End stack frame ID (typically not used for instant events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esf: Option<u64>,
}

/// Counter event for tracking values over time.
///
/// Counter events can track a value or multiple values as they change over time.
/// They are displayed as line graphs in the trace viewer.
///
/// Key features:
/// - Multiple series supported (shown as stacked area chart)
/// - Counters are process-local
/// - When `id` field exists, the combination of name + id identifies the counter
/// - Each key in args represents a different data series
///
/// Example args: `{"cats": 10, "dogs": 5}` creates two series
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct CounterEvent {
    /// Display name of the counter in the trace viewer.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be Counter (C).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Counter values as key-value pairs. Each key is a series name.
    ///
    /// Required field. Each property becomes a series in the counter graph.
    /// Values must be numeric. When multiple series exist, they're displayed
    /// as a stacked area chart with the sum shown.
    pub args: Value,
    /// Optional ID to distinguish multiple counters with the same name.
    ///
    /// When provided, the counter is identified by the combination of
    /// name + id. This allows multiple independent counters with the
    /// same name in a single process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
}

/// Async event for tracking asynchronous operations.
///
/// Async events are used to specify asynchronous operations like network I/O,
/// disk operations, or cross-process communications. Key features:
///
/// - Can span across different threads and processes
/// - Events with same category + id form an async operation tree
/// - Parent-child relationships inferred from timestamp nesting
/// - Support nested async operations (e.g., HTTP request → headers → body)
///
/// Phases:
/// - **b** (begin): Start of an async operation
/// - **n** (nestable instant): Intermediate step or nested event
/// - **e** (end): End of an async operation
///
/// The root of an async tree is drawn with a dark top border in the viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncEvent {
    /// Display name of the async operation.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be AsyncBegin (b), AsyncStep (n), or AsyncEnd (e).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Async event ID. Events with same category and ID form an async tree.
    ///
    /// Required field. All events with the same category and ID are considered
    /// part of the same async operation. By default, async IDs are global
    /// across processes (unlike object IDs which are process-local).
    pub id: Id,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Scope for ID disambiguation.
    ///
    /// Optional string to avoid ID conflicts. Events with the same ID but
    /// different scopes are treated as separate entities. Useful when IDs
    /// might collide across different subsystems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    /// ID for binding flow events to this async event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_id: Option<String>,
    /// Indicates this async event is a flow target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_in: Option<bool>,
    /// Indicates this async event is a flow source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_out: Option<bool>,
    /// Explicit local/global identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id2: Option<Id2>,
    /// Stack frame ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sf: Option<u64>,
    /// End stack frame ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esf: Option<u64>,
}

/// Flow event showing relationships between events across threads/processes.
///
/// Flow events are similar to async events but create visual arrows between
/// duration events to show causal relationships. Think of them as arrows
/// connecting cause and effect across threads.
///
/// Binding rules:
/// - **s** (start): Binds to enclosing slice
/// - **t** (step): Binds to enclosing slice  
/// - **f** (end): Binds to next slice (or enclosing if bp="e")
///
/// "Next slice" means the next slice to begin >= the flow event timestamp.
/// If multiple slices start at the same time, the earliest in the trace
/// buffer is chosen. Invalid if no valid binding target exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowEvent {
    /// Display name of the flow in the trace viewer.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be FlowBegin (s), FlowStep (t), or FlowEnd (f).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Flow event ID. Events with same ID form a flow sequence.
    ///
    /// Required field. All flow events with the same ID are connected
    /// with arrows in the trace viewer. The ID links the flow start,
    /// steps, and end together.
    pub id: Id,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// ID for explicit binding to a slice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_id: Option<String>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
    /// Indicates this is also a flow target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_in: Option<bool>,
    /// Indicates this is also a flow source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_out: Option<bool>,
    /// Explicit local/global identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id2: Option<Id2>,
    /// Scope for ID disambiguation.
    ///
    /// Optional string to avoid ID conflicts. Events with the same ID but
    /// different scopes are treated as separate entities. Useful when IDs
    /// might collide across different subsystems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Stack frame ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sf: Option<u64>,
    /// End stack frame ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esf: Option<u64>,
}

/// Sample event from a sampling profiler (deprecated).
///
/// Sample events provide sampling-profiler results in the trace. They appear
/// as vertical lines (0 duration) similar to instant events.
///
/// **Deprecated**: Use the global `samples` array instead for better performance
/// and integration with OS-level profilers.
///
/// Useful for hot function analysis without overwhelming the trace with
/// thousands of duration events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleEvent {
    /// Display name of the sample.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be Sample (P).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Stack frame ID referencing the stackFrames dictionary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sf: Option<u64>,
    /// Sample weight for aggregation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u64>,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
}

/// Object event for tracking object lifecycle and state.
///
/// Object events track complex data structures over time:
/// - **N** (created): Object instance creation
/// - **O** (snapshot): Capture object state at a point in time
/// - **D** (destroyed): Object instance destruction
///
/// Key concepts:
/// - IDs (usually pointers like "0x1000") can be reused after destruction
/// - Lifetimes are inclusive of start, exclusive of end time
/// - Object IDs are process-local by default
/// - Snapshots can have custom viewers in the trace viewer
/// - Use `scope` field to avoid ID conflicts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectEvent {
    /// Object type name.
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be ObjectCreated (N), ObjectSnapshot (O), or ObjectDestroyed (D).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Object ID. By default, object IDs are process-local.
    pub id: Id,
    /// For snapshots: contains the snapshot data. Not used for N/D events.
    ///
    /// Must contain a `snapshot` property with the actual snapshot data.
    /// The snapshot can override its category with `snapshot.cat` and
    /// base type with `snapshot.base_type` for polymorphic objects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Scope for ID disambiguation.
    ///
    /// Optional string to avoid ID conflicts. Events with the same ID but
    /// different scopes are treated as separate entities. Useful when IDs
    /// might collide across different subsystems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
}

/// Metadata event names for process and thread information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetadataName {
    /// Sets the display name for a thread.
    #[serde(rename = "thread_name")]
    ThreadName,
    /// Sets the sort order position for a thread.
    #[serde(rename = "thread_sort_index")]
    ThreadSortIndex,
    /// Sets the display name for a process.
    #[serde(rename = "process_name")]
    ProcessName,
    /// Sets extra labels for a process.
    #[serde(rename = "process_labels")]
    ProcessLabels,
    /// Sets the sort order position for a process.
    #[serde(rename = "process_sort_index")]
    ProcessSortIndex,
    /// Sets the process uptime in seconds.
    #[serde(rename = "process_uptime_seconds")]
    ProcessUptimeSeconds,
    /// Screenshot data.
    #[serde(rename = "screenshots")]
    Screenshots,
    /// Stack frame data.
    #[serde(rename = "stackFrames")]
    StackFrames,
}

/// Metadata event for providing process and thread names and other information.
///
/// Metadata events associate extra information with processes and threads:
/// - Process/thread display names
/// - Sort order for process/thread lists
/// - Process labels and uptime
///
/// Sort indices control display order (lower = higher in the UI).
/// Items with same sort index are sorted by name, then by ID.
#[derive(Debug, Clone, Serialize, Deserialize, Builder)]
pub struct MetadataEvent {
    /// Event phase - must be Metadata (M).
    pub ph: Phase,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID (required for thread metadata, optional for process metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tid: Option<u32>,
    /// Type of metadata being set.
    pub name: MetadataName,
    /// Metadata value (e.g., {"name": "MainThread"} for thread_name).
    ///
    /// The structure depends on the metadata type:
    /// - thread_name/process_name: `{"name": "..."}`
    /// - thread_sort_index/process_sort_index: `{"sort_index": number}`
    /// - process_labels: `{"labels": "comma,separated,labels"}`
    pub args: Value,
}

/// Memory dump event for recording memory usage information.
///
/// Memory dumps capture memory state at specific points in time:
/// - **V** (global): System-wide memory info (e.g., total RAM)
/// - **v** (process): Per-process memory usage
///
/// All dumps with the same dump ID represent a simultaneous snapshot
/// across the system. Global dumps (0 or 1) and process dumps (0 or more)
/// are correlated by their dump ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDumpEvent {
    /// Event phase - must be GlobalMemoryDump (V) or ProcessMemoryDump (v).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Memory dump data (format depends on dump type).
    ///
    /// The structure of this data depends on whether this is a global (V)
    /// or process (v) memory dump. Contains memory statistics, allocator
    /// information, and other memory-related metrics.
    pub args: Value,
    /// Dump ID to correlate global and process dumps.
    ///
    /// All memory dump events with the same ID represent a simultaneous
    /// snapshot across the system. Used to correlate global and per-process
    /// memory information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Comma-separated list of categories for filtering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
}

/// Mark event for navigation timing and user-defined marks.
///
/// Mark events are created in two ways:
/// 1. Automatically at key web page lifecycle points:
///    - navigationStart, fetchStart, domComplete, loadEventEnd, etc.
/// 2. Programmatically via JavaScript:
///    - `performance.mark('myCustomMark')`
///
/// Allows annotation of domain-specific events (e.g., 'searchComplete',
/// 'firstMeaningfulPaint'). Typically in category 'blink.user_timing'.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkEvent {
    /// Mark name (e.g., "navigationStart", "domComplete").
    pub name: String,
    /// Comma-separated list of categories for filtering in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Event phase - must be Mark (R).
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
    /// Fixed color name from the trace viewer's color scheme.
    ///
    /// Must be one of the names from trace-viewer's base color scheme's
    /// reserved color names list. Overrides the default color assignment
    /// based on the event name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cname: Option<String>,
}

/// Clock synchronization event for multi-agent traces.
///
/// Clock sync events enable synchronization of timestamps across different
/// clock domains (e.g., different machines, processes, or hardware clocks).
///
/// Two types:
/// 1. **Receiver**: Records when sync marker received (only sync_id in args)
/// 2. **Issuer**: Records round-trip time (sync_id + issue_ts in args)
///
/// For issuers, `ts - issue_ts` represents the round-trip latency,
/// used for more precise clock alignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClockSyncEvent {
    /// Event name - must be "clock_sync".
    ///
    /// This field must always contain the string "clock_sync" for clock
    /// synchronization events to be properly recognized.
    pub name: String,
    /// Event phase - must be ClockSync (c).
    pub ph: Phase,
    /// Timestamp in microseconds (receiver's clock or issuer's end time).
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Contains sync_id and optionally issue_ts.
    ///
    /// Required fields:
    /// - `sync_id`: String ID for the clock sync marker
    /// - `issue_ts`: (issuer only) Timestamp when marker was issued
    ///
    /// Example: `{"sync_id": "guid-1234", "issue_ts": 5000}`
    pub args: Value,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
}

/// Context event for grouping related events.
///
/// Context events mark sequences of trace events as belonging to a particular
/// context (or tree of contexts). All events on a thread between enter '('
/// and leave ')' belong to that context.
///
/// Contexts can be nested and form trees. Context IDs refer to context
/// object snapshots. Useful for attributing events to higher-level
/// operations or frameworks (e.g., "handling click event").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEvent {
    /// Event phase - must be ContextEnter '(' or ContextLeave ')'.
    pub ph: Phase,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// Context ID referencing a context object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Comma-separated list of categories for filtering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
}

/// Event for linking two IDs together.
///
/// Specifies that two different IDs refer to the same logical entity.
/// This is useful when:
/// - An async operation changes IDs during its lifetime
/// - Events are traced with different IDs in different components
/// - Correlating IDs across system boundaries
///
/// The event's `id` field contains the first ID, and `args.linked_id`
/// contains the second ID that should be treated as equivalent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedIdEvent {
    /// Event phase - must be LinkedId '='.
    pub ph: Phase,
    /// Timestamp in microseconds (optional for linked ID events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
    /// Process ID.
    ///
    /// The process ID for the process that output this event. Used to group
    /// events by process in the trace viewer.
    pub pid: u32,
    /// Thread ID.
    ///
    /// The thread ID for the thread that output this event. Events are grouped
    /// by thread within each process in the trace viewer.
    pub tid: u32,
    /// First ID.
    pub id: Id,
    /// Second ID that is linked to the first.
    ///
    /// The ID that should be treated as equivalent to the event's main `id`.
    /// After this event, references to either ID will be treated as the same
    /// logical entity.
    pub linked_id: Id,
    /// Custom arguments displayed in the trace viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    /// Comma-separated list of categories for filtering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat: Option<String>,
    /// Thread clock timestamp in microseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts: Option<u64>,
}

/// Dictionary mapping stack frame IDs to stack frame information.
///
/// The stackFrames dictionary enables compact representation of stack traces
/// by storing each unique frame once. Benefits:
/// - Dramatically reduces trace file size for stack-heavy traces
/// - Allows efficient deduplication of common frames
/// - Preserves full call stack information
///
/// Stack frames form a tree structure via parent references, with roots
/// having no parent field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackFrames(pub std::collections::HashMap<String, StackFrame>);

/// Stack frame information for a single frame in a call stack.
///
/// Represents one frame in a call stack. Frames are linked via parent
/// references to form complete stack traces. The trace viewer uses this
/// information to display call stacks and aggregate by function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackFrame {
    /// Function or method name for this frame.
    ///
    /// The name displayed for this stack frame. Can be a function name,
    /// method name, or any string identifying this code location.
    /// Examples: "malloc", "MyClass::processData", "<anonymous>".
    pub name: String,
    /// Category for this frame (e.g., module or library name).
    ///
    /// Used to group frames by component. Common examples:
    /// - Shared library: "libc.so", "user32.dll"
    /// - Component: "v8", "blink", "net"
    /// - Language runtime: "[jit]", "[interpreted]"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Parent frame ID, forming a tree structure. Omit for root frames.
    ///
    /// References another frame in the stackFrames dictionary that called
    /// this frame. The chain of parent references forms the complete stack.
    /// Root frames (e.g., main, thread entry points) have no parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// Global sample from OS-level profiling.
///
/// Global samples store OS-level sampling profiler data (e.g., from perf, ETW).
/// They augment trace events with lower-level CPU profiling information.
///
/// Unlike deprecated sample events (P), global samples:
/// - Are stored in a dedicated array for efficiency  
/// - Include CPU core information
/// - Support hardware performance counters
/// - Integrate better with system profilers
///
/// Samples are weighted for statistical aggregation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// CPU core number where the sample was taken.
    ///
    /// Identifies which CPU core was executing when the sample was collected.
    /// Useful for analyzing CPU migrations and core utilization. May be
    /// undefined if CPU information is not available.
    pub cpu: u32,
    /// Thread ID that was running when sampled.
    pub tid: u32,
    /// Timestamp in microseconds.
    pub ts: u64,
    /// Counter name (e.g., "cycles:HG" for hardware cycles).
    ///
    /// Identifies what performance counter triggered this sample. Examples:
    /// - "cycles:HG" - CPU cycles (hardware, guest mode)
    /// - "instructions" - Retired instructions  
    /// - "cache-misses" - Cache miss events
    /// - "branch-misses" - Branch mispredictions
    pub name: String,
    /// Stack frame ID referencing the stackFrames dictionary.
    pub sf: u64,
    /// Sample weight for aggregation (default: 1).
    ///
    /// The weight represents the relative importance of this sample.
    /// Used when aggregating samples to determine hot spots. Higher
    /// weights indicate more time spent or more events occurring.
    /// Default weight is 1 if not specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u64>,
}
