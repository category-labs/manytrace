use perfetto_format::{
    create_callstack_interned_data, create_debug_annotation, DebugValue, PerfettoStreamWriter,
};
use std::fs::File;
use std::io::BufWriter;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn timestamp_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn main() -> anyhow::Result<()> {
    let file = File::create("streaming_example.pftrace")?;
    let buffered_writer = BufWriter::new(file);
    let mut writer = PerfettoStreamWriter::new(buffered_writer);

    println!("Starting streaming trace...");

    let process_track_uuid = writer
        .write_process_descriptor(std::process::id(), Some("streaming_example".to_string()))?;

    let main_thread_track_uuid = writer.write_thread_descriptor(
        thread_id::get() as u32,
        Some("main_thread".to_string()),
        process_track_uuid,
    )?;

    let callstack_data = create_callstack_interned_data(
        0,
        vec![
            ("main", "main.rs", 0x400000),
            ("process_batch", "processor.rs", 0x401000),
            ("compute", "compute.rs", 0x402000),
            ("inner_loop", "compute.rs", 0x402100),
        ],
    );
    writer.write_interned_data(callstack_data)?;

    println!("Writing events in real-time...");

    let main_start = timestamp_nanos();
    writer.write_slice_begin(
        main_thread_track_uuid,
        "streaming_session".to_string(),
        main_start,
        vec![create_debug_annotation(
            "total_iterations".to_string(),
            DebugValue::Int(100),
        )],
    )?;

    for i in 0..100 {
        let timestamp = timestamp_nanos();

        if i % 10 == 0 {
            let batch_start = timestamp;
            writer.write_slice_begin(
                main_thread_track_uuid,
                format!("process_batch_{}", i / 10),
                batch_start,
                vec![create_debug_annotation(
                    "batch_number".to_string(),
                    DebugValue::Int(i / 10),
                )],
            )?;

            writer.write_instant_event(
                main_thread_track_uuid,
                format!("checkpoint_{}", i),
                timestamp,
                vec![
                    create_debug_annotation("iteration".to_string(), DebugValue::Int(i)),
                    create_debug_annotation(
                        "progress".to_string(),
                        DebugValue::Double((i as f64) / 100.0),
                    ),
                ],
            )?;
            println!("Written checkpoint {}", i);
        }

        if i % 5 == 0 {
            let iter_start = timestamp;
            writer.write_slice_begin(
                main_thread_track_uuid,
                "compute_iteration".to_string(),
                iter_start,
                vec![],
            )?;

            writer.write_perf_sample(
                0,
                std::process::id(),
                thread_id::get() as u32,
                timestamp + 1_000_000,
                0,
                perfetto_format::perfetto::profiling::CpuMode::ModeUser,
            )?;

            writer.write_slice_end(main_thread_track_uuid, timestamp + 2_000_000)?;
        }

        if i % 10 == 9 {
            writer.write_slice_end(main_thread_track_uuid, timestamp + 5_000_000)?;
        }

        if i % 25 == 0 && i > 0 {
            writer.write_instant_event(
                main_thread_track_uuid,
                "quarter_complete".to_string(),
                timestamp,
                vec![create_debug_annotation(
                    "percentage".to_string(),
                    DebugValue::Int(i * 100 / 100),
                )],
            )?;
        }

        thread::sleep(Duration::from_millis(10));
    }

    writer.write_slice_end(main_thread_track_uuid, timestamp_nanos())?;

    writer.write_instant_event(
        main_thread_track_uuid,
        "trace_complete".to_string(),
        timestamp_nanos(),
        vec![
            create_debug_annotation("total_samples".to_string(), DebugValue::Int(100)),
            create_debug_annotation(
                "status".to_string(),
                DebugValue::String("success".to_string()),
            ),
        ],
    )?;

    writer.flush()?;

    println!("Streaming trace completed!");
    println!("Trace written to: streaming_example.pftrace");
    println!("You can view this trace at https://ui.perfetto.dev");

    Ok(())
}
