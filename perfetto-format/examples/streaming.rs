use perfetto_format::{create_debug_annotation, DebugValue, PerfettoStreamWriter};
use std::fs::File;
use std::io::BufWriter;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{process, thread};

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
        process::id(),
        thread_id::get() as u32,
        Some("main_thread".to_string()),
        process_track_uuid,
    )?;

    writer.write_callstack_interned_data(0, vec![("AA", "A", 0x400000), ("BB", "B", 0x401000)])?;

    writer.write_callstack_interned_data(1, vec![("ZZ", "Z", 0x500000), ("XX", "X", 0x501000)])?;

    let memory_counter_track = writer.write_counter_track(
        "Memory Usage".to_string(),
        Some("bytes".to_string()),
        main_thread_track_uuid,
    )?;

    let cpu_counter_track = writer.write_counter_track(
        "CPU Usage".to_string(),
        Some("%".to_string()),
        main_thread_track_uuid,
    )?;

    let items_processed_track =
        writer.write_counter_track("Items Processed".to_string(), None, main_thread_track_uuid)?;

    let network_bytes_track = writer.write_counter_track(
        "Network Bytes Sent".to_string(),
        Some("bytes".to_string()),
        main_thread_track_uuid,
    )?;

    let queue_size_track = writer.write_counter_track(
        "Queue Size".to_string(),
        Some("items".to_string()),
        main_thread_track_uuid,
    )?;

    let error_count_track =
        writer.write_counter_track("Error Count".to_string(), None, main_thread_track_uuid)?;

    let latency_track = writer.write_counter_track(
        "Processing Latency".to_string(),
        Some("ms".to_string()),
        main_thread_track_uuid,
    )?;

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

    let mut base_memory = 1024 * 1024 * 100;
    let mut items_processed = 0i64;
    let mut network_bytes = 0i64;
    let mut error_count = 0i64;

    for i in 0..100 {
        let timestamp = timestamp_nanos();

        items_processed += 1;
        writer.write_counter_value(items_processed_track, items_processed, timestamp)?;

        let memory_usage = base_memory + (i * 1024 * 512);
        writer.write_counter_value(memory_counter_track, memory_usage, timestamp)?;

        let cpu_usage = 20.0 + 30.0 * ((i as f64 * 0.1).sin() + 1.0);
        writer.write_double_counter_value(cpu_counter_track, cpu_usage, timestamp)?;

        let bytes_sent = 1024 + (i * 100);
        network_bytes += bytes_sent;
        writer.write_counter_value(network_bytes_track, network_bytes, timestamp)?;

        let queue_size = ((i as f64 * 0.2).sin() * 50.0 + 50.0) as i64;
        writer.write_counter_value(queue_size_track, queue_size, timestamp)?;

        if i % 17 == 0 && i > 0 {
            error_count += 1;
            writer.write_counter_value(error_count_track, error_count, timestamp)?;
        }

        let latency = 10.0 + 5.0 * ((i as f64 * 0.15).cos() + 1.0);
        writer.write_double_counter_value(latency_track, latency, timestamp)?;

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
            writer.write_perf_sample(
                0,
                std::process::id(),
                thread_id::get() as u32,
                timestamp + 1_500_000,
                1,
                perfetto_format::perfetto::profiling::CpuMode::ModeUser,
            )?;

            writer.write_slice_end(main_thread_track_uuid, timestamp + 2_000_000)?;
        }

        if i % 10 == 9 {
            writer.write_slice_end(main_thread_track_uuid, timestamp + 5_000_000)?;

            base_memory += 1024 * 1024 * 5;
            let spike_timestamp = timestamp + 5_500_000;
            writer.write_counter_value(memory_counter_track, base_memory, spike_timestamp)?;
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

    let final_timestamp = timestamp_nanos();

    writer.write_counter_value(items_processed_track, items_processed, final_timestamp)?;
    writer.write_counter_value(memory_counter_track, base_memory, final_timestamp)?;
    writer.write_double_counter_value(cpu_counter_track, 0.0, final_timestamp)?;
    writer.write_counter_value(network_bytes_track, network_bytes, final_timestamp)?;
    writer.write_counter_value(queue_size_track, 0, final_timestamp)?;
    writer.write_counter_value(error_count_track, error_count, final_timestamp)?;
    writer.write_double_counter_value(latency_track, 0.0, final_timestamp)?;

    writer.write_instant_event(
        main_thread_track_uuid,
        "trace_complete".to_string(),
        final_timestamp,
        vec![
            create_debug_annotation("total_samples".to_string(), DebugValue::Int(100)),
            create_debug_annotation(
                "items_processed".to_string(),
                DebugValue::Int(items_processed),
            ),
            create_debug_annotation(
                "final_memory_mb".to_string(),
                DebugValue::Int(base_memory / (1024 * 1024)),
            ),
            create_debug_annotation(
                "network_mb_sent".to_string(),
                DebugValue::Double(network_bytes as f64 / (1024.0 * 1024.0)),
            ),
            create_debug_annotation("error_count".to_string(), DebugValue::Int(error_count)),
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
