use perfetto_format::PerfettoStreamWriter;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::time::{SystemTime, UNIX_EPOCH};

fn timestamp_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

#[derive(Hash, Eq, PartialEq, Clone)]
struct CallstackKey {
    frames: Vec<u64>,
}

struct CallstackCache {
    mapping: HashMap<CallstackKey, u64>,
    sequence: u64,
}

impl CallstackCache {
    fn new() -> Self {
        CallstackCache {
            mapping: HashMap::new(),
            sequence: 0,
        }
    }

    fn get_or_insert(&mut self, frames: Vec<u64>) -> (u64, bool) {
        let key = CallstackKey { frames };

        if let Some(&id) = self.mapping.get(&key) {
            (id, true)
        } else {
            let id = self.sequence;
            self.mapping.insert(key, id);
            self.sequence += 1;
            (id, false)
        }
    }
}

fn create_callstack_frames(pattern: &str) -> Vec<(&'static str, &'static str, u64)> {
    match pattern {
        "main_loop" => vec![
            ("main", "example", 0x400000),
            ("run_loop", "example", 0x401000),
            ("process_items", "example", 0x402000),
        ],
        "worker_a" => vec![
            ("main", "example", 0x400000),
            ("worker_thread", "example", 0x403000),
            ("do_work_a", "example", 0x404000),
            ("compute_result", "example", 0x405000),
        ],
        "worker_b" => vec![
            ("main", "example", 0x400000),
            ("worker_thread", "example", 0x403000),
            ("do_work_b", "example", 0x406000),
            ("process_data", "example", 0x407000),
        ],
        "deep_stack" => vec![
            ("main", "example", 0x400000),
            ("level_1", "example", 0x408000),
            ("level_2", "example", 0x409000),
            ("level_3", "example", 0x40a000),
            ("level_4", "example", 0x40b000),
            ("level_5", "example", 0x40c000),
        ],
        _ => vec![("unknown", "example", 0x500000)],
    }
}

fn main() -> anyhow::Result<()> {
    let file = File::create("/tmp/callstacks.perfetto")?;
    let buffered_writer = BufWriter::new(file);
    let mut writer = PerfettoStreamWriter::new(buffered_writer);

    println!("Starting callstack caching demonstration...");

    let mut cache = CallstackCache::new();
    let mut cache_hits = 0;
    let mut cache_misses = 0;

    let _patterns = [
        ("main_loop", 30),
        ("worker_a", 20),
        ("worker_b", 20),
        ("deep_stack", 10),
    ];

    let start_time = timestamp_nanos();

    let mut sample_count = 0;

    for iteration in 0..100 {
        let timestamp = start_time + (iteration * 1_000_000);

        let pattern = if iteration % 3 == 0 {
            "main_loop"
        } else if iteration % 4 == 0 {
            "worker_a"
        } else if iteration % 5 == 0 {
            "worker_b"
        } else if iteration % 10 == 0 {
            "deep_stack"
        } else {
            "main_loop"
        };

        let frames_data = create_callstack_frames(pattern);
        let frames: Vec<u64> = frames_data.iter().map(|(_, _, addr)| *addr).collect();

        let (callstack_id, is_cached) = cache.get_or_insert(frames.clone());

        if is_cached {
            cache_hits += 1;
        } else {
            cache_misses += 1;
            writer.write_callstack_interned_data(callstack_id, frames_data)?;
        }

        writer.write_perf_sample(
            0,
            std::process::id(),
            1234,
            timestamp,
            callstack_id,
            perfetto_format::perfetto::profiling::CpuMode::ModeUser,
        )?;

        sample_count += 1;

        if iteration % 20 == 0 {
            println!(
                "Progress: {} samples, {} cache hits, {} cache misses (hit rate: {:.1}%)",
                sample_count,
                cache_hits,
                cache_misses,
                (cache_hits as f64 / (cache_hits + cache_misses) as f64) * 100.0
            );
        }
    }

    writer.flush()?;

    println!("\nCallstack caching demonstration completed!");
    println!("Trace written to: /tmp/callstacks.perfetto");
    println!("\nFinal Statistics:");
    println!("  Total samples: {}", sample_count);
    println!("  Unique callstacks: {}", cache.mapping.len());
    println!("  Cache hits: {}", cache_hits);
    println!("  Cache misses: {}", cache_misses);
    println!(
        "  Cache hit rate: {:.1}%",
        (cache_hits as f64 / (cache_hits + cache_misses) as f64) * 100.0
    );
    println!(
        "  Memory saved: {:.1}%",
        (cache_hits as f64 / sample_count as f64) * 100.0
    );
    println!("\nYou can view this trace at https://ui.perfetto.dev");

    Ok(())
}
