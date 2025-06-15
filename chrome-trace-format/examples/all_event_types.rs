//! Example demonstrating Chrome trace event types supported by Perfetto with overlapping events.
//!
//! This example creates a trace file with Duration, Complete, Counter, and Async events,
//! showing how they interact and overlap in the timeline. Event names include their type for clarity.
//!
//! Usage: all_event_types <output_file>

use chrome_trace_format::*;
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let output_file = if args.len() > 1 {
        &args[1]
    } else {
        eprintln!("Usage: {} <output_file>", args[0]);
        std::process::exit(1);
    };

    let mut events = vec![];

    // Metadata events
    events.push(TraceEvent::Metadata(MetadataEvent {
        ph: Phase::Metadata,
        pid: 1234,
        tid: Some(5678),
        name: MetadataName::ThreadName,
        args: serde_json::json!({"name": "MainThread"}),
    }));

    events.push(TraceEvent::Metadata(MetadataEvent {
        ph: Phase::Metadata,
        pid: 1234,
        tid: Some(5679),
        name: MetadataName::ThreadName,
        args: serde_json::json!({"name": "WorkerThread1"}),
    }));

    events.push(TraceEvent::Metadata(MetadataEvent {
        ph: Phase::Metadata,
        pid: 1234,
        tid: Some(5680),
        name: MetadataName::ThreadName,
        args: serde_json::json!({"name": "WorkerThread2"}),
    }));

    events.push(TraceEvent::Metadata(MetadataEvent {
        ph: Phase::Metadata,
        pid: 1234,
        tid: None,
        name: MetadataName::ProcessName,
        args: serde_json::json!({"name": "MyApp"}),
    }));

    // Main duration event
    events.push(TraceEvent::Duration(DurationEvent {
        name: "Duration_mainFunction".to_string(),
        cat: Some("app".to_string()),
        ph: Phase::DurationBegin,
        ts: 1000000,
        pid: 1234,
        tid: 5678,
        args: Some(serde_json::json!({"input": "start"})),
        dur: None,
        tdur: None,
        tts: Some(100000),
        cname: Some("thread_state_running".to_string()),
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Counter event at the start
    events.push(TraceEvent::Counter(CounterEvent {
        name: "Counter_memoryUsage".to_string(),
        cat: Some("memory".to_string()),
        ph: Phase::Counter,
        ts: 1000100,
        pid: 1234,
        tid: 5678,
        args: serde_json::json!({
            "heap": 1024000,
            "stack": 256000
        }),
        id: None,
        tts: None,
        cname: None,
    }));

    // Nested duration event
    events.push(TraceEvent::Duration(DurationEvent {
        name: "Duration_nestedFunction".to_string(),
        cat: Some("app".to_string()),
        ph: Phase::DurationBegin,
        ts: 1000500,
        pid: 1234,
        tid: 5678,
        args: Some(serde_json::json!({"nested": true})),
        dur: None,
        tdur: None,
        tts: Some(100500),
        cname: Some("thread_state_running".to_string()),
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Complete event overlapping with durations
    events.push(TraceEvent::Complete(CompleteEvent {
        name: "Complete_processData".to_string(),
        cat: Some("processing".to_string()),
        ph: Phase::Complete,
        ts: 1000600,
        dur: 300,
        pid: 1234,
        tid: 5678,
        args: Some(serde_json::json!({"items": 100})),
        tdur: Some(280),
        tts: Some(100600),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Async event starting on main thread
    events.push(TraceEvent::Async(AsyncEvent {
        name: "Async_networkRequest".to_string(),
        cat: Some("network".to_string()),
        ph: Phase::AsyncBegin,
        ts: 1000700,
        pid: 1234,
        tid: 5678,
        id: Id::String("req1".to_string()),
        args: Some(serde_json::json!({"url": "https://api.example.com/data"})),
        scope: None,
        tts: Some(100700),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id2: None,
        sf: None,
        esf: None,
    }));

    // Counter update
    events.push(TraceEvent::Counter(CounterEvent {
        name: "Counter_memoryUsage".to_string(),
        cat: Some("memory".to_string()),
        ph: Phase::Counter,
        ts: 1001000,
        pid: 1234,
        tid: 5678,
        args: serde_json::json!({
            "heap": 1124000,
            "stack": 260000
        }),
        id: None,
        tts: None,
        cname: None,
    }));

    // End nested duration
    events.push(TraceEvent::Duration(DurationEvent {
        name: "Duration_nestedFunction".to_string(),
        cat: Some("app".to_string()),
        ph: Phase::DurationEnd,
        ts: 1001200,
        pid: 1234,
        tid: 5678,
        args: Some(serde_json::json!({"result": "success"})),
        dur: None,
        tdur: None,
        tts: Some(101200),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Parallel work on different thread - Duration
    events.push(TraceEvent::Duration(DurationEvent {
        name: "Duration_workerTask".to_string(),
        cat: Some("worker".to_string()),
        ph: Phase::DurationBegin,
        ts: 1001000,
        pid: 1234,
        tid: 5679,
        args: Some(serde_json::json!({"workerId": 1})),
        dur: None,
        tdur: None,
        tts: Some(200000),
        cname: Some("thread_state_running".to_string()),
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Complete event on worker thread
    events.push(TraceEvent::Complete(CompleteEvent {
        name: "Complete_computation".to_string(),
        cat: Some("compute".to_string()),
        ph: Phase::Complete,
        ts: 1001100,
        dur: 400,
        pid: 1234,
        tid: 5679,
        args: Some(serde_json::json!({"complexity": "high"})),
        tdur: Some(380),
        tts: Some(200100),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Async step
    events.push(TraceEvent::Async(AsyncEvent {
        name: "Async_networkRequest".to_string(),
        cat: Some("network".to_string()),
        ph: Phase::AsyncStep,
        ts: 1001300,
        pid: 1234,
        tid: 5680,
        id: Id::String("req1".to_string()),
        args: Some(serde_json::json!({"step": "processing response"})),
        scope: None,
        tts: Some(300000),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id2: None,
        sf: None,
        esf: None,
    }));

    // Another async event (database query)
    events.push(TraceEvent::Async(AsyncEvent {
        name: "Async_databaseQuery".to_string(),
        cat: Some("database".to_string()),
        ph: Phase::AsyncBegin,
        ts: 1001400,
        pid: 1234,
        tid: 5680,
        id: Id::String("db1".to_string()),
        args: Some(serde_json::json!({"query": "SELECT * FROM users"})),
        scope: None,
        tts: Some(300100),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id2: None,
        sf: None,
        esf: None,
    }));

    // CPU usage counter
    events.push(TraceEvent::Counter(CounterEvent {
        name: "Counter_cpuUsage".to_string(),
        cat: Some("system".to_string()),
        ph: Phase::Counter,
        ts: 1001500,
        pid: 1234,
        tid: 5678,
        args: serde_json::json!({
            "usage_percent": 75
        }),
        id: None,
        tts: None,
        cname: None,
    }));

    // End worker task
    events.push(TraceEvent::Duration(DurationEvent {
        name: "Duration_workerTask".to_string(),
        cat: Some("worker".to_string()),
        ph: Phase::DurationEnd,
        ts: 1001800,
        pid: 1234,
        tid: 5679,
        args: Some(serde_json::json!({"completed": true})),
        dur: None,
        tdur: None,
        tts: Some(200800),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Async end for network request
    events.push(TraceEvent::Async(AsyncEvent {
        name: "Async_networkRequest".to_string(),
        cat: Some("network".to_string()),
        ph: Phase::AsyncEnd,
        ts: 1002000,
        pid: 1234,
        tid: 5680,
        id: Id::String("req1".to_string()),
        args: Some(serde_json::json!({"status": 200, "bytes": 4096})),
        scope: None,
        tts: Some(300200),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id2: None,
        sf: None,
        esf: None,
    }));

    // End main function
    events.push(TraceEvent::Duration(DurationEvent {
        name: "Duration_mainFunction".to_string(),
        cat: Some("app".to_string()),
        ph: Phase::DurationEnd,
        ts: 1002100,
        pid: 1234,
        tid: 5678,
        args: Some(serde_json::json!({"output": "complete"})),
        dur: None,
        tdur: None,
        tts: Some(102100),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id: None,
        id2: None,
        scope: None,
        sf: None,
        esf: None,
    }));

    // Final counter
    events.push(TraceEvent::Counter(CounterEvent {
        name: "Counter_memoryUsage".to_string(),
        cat: Some("memory".to_string()),
        ph: Phase::Counter,
        ts: 1002200,
        pid: 1234,
        tid: 5678,
        args: serde_json::json!({
            "heap": 1024000,
            "stack": 256000
        }),
        id: None,
        tts: None,
        cname: None,
    }));

    // End database query
    events.push(TraceEvent::Async(AsyncEvent {
        name: "Async_databaseQuery".to_string(),
        cat: Some("database".to_string()),
        ph: Phase::AsyncEnd,
        ts: 1002300,
        pid: 1234,
        tid: 5680,
        id: Id::String("db1".to_string()),
        args: Some(serde_json::json!({"rows": 42})),
        scope: None,
        tts: Some(300300),
        cname: None,
        bind_id: None,
        flow_in: None,
        flow_out: None,
        id2: None,
        sf: None,
        esf: None,
    }));

    let trace = ChromeTrace {
        trace_events: Some(events),
        display_time_unit: Some("ms".to_string()),
        system_trace_events: None,
        other_data: None,
        power_trace_asynchronous_data: None,
        stack_frames: None,
        samples: None,
        control_flow_profile: None,
        metadata: Some(serde_json::json!({
            "command_line": ["my_app", "--trace"],
            "version": "1.0"
        })),
    };

    let json = serde_json::to_string_pretty(&trace)?;
    std::fs::write(output_file, &json)?;
    println!(
        "trace with perfetto-supported events written to {}",
        output_file
    );
    Ok(())
}
