use bpf::BpfConfig;
use clap::Parser;
use eyre::Result;
use perfetto_format::{
    create_callstack_interned_data, create_debug_annotation,
    perfetto::profiling::CpuMode as PerfettoCpuMode, DebugValue, PerfettoStreamWriter,
};
use protocol::{CpuMode, Event};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread::sleep;
use std::time::Duration;
use tracing::debug;

#[derive(Parser, Debug)]
#[command(name = "bpftrace")]
#[command(about = "Collect BPF profiling data and convert to Perfetto format", long_about = None)]
struct Args {
    #[arg(help = "Configuration file path (TOML format)")]
    config: PathBuf,

    #[arg(short, long, help = "Output file path")]
    output: PathBuf,

    #[arg(
        short,
        long,
        default_value = "5",
        help = "Duration to collect events in seconds"
    )]
    duration: u64,
}

struct ProfilerConverter {
    writer: PerfettoStreamWriter<BufWriter<File>>,
    process_tracks: HashMap<i32, u64>,
    thread_tracks: HashMap<(i32, i32), u64>,
    counter_tracks: HashMap<String, u64>,
}

impl ProfilerConverter {
    fn new(output_path: PathBuf) -> Result<Self> {
        let file = File::create(output_path)?;
        let buffered_writer = BufWriter::new(file);
        let writer = PerfettoStreamWriter::new(buffered_writer);

        Ok(Self {
            writer,
            process_tracks: HashMap::new(),
            thread_tracks: HashMap::new(),
            counter_tracks: HashMap::new(),
        })
    }

    fn ensure_process_track(&mut self, pid: i32) -> Result<u64> {
        if let Some(&track_uuid) = self.process_tracks.get(&pid) {
            return Ok(track_uuid);
        }

        let track_uuid = self.writer.write_process_descriptor(
            pid as u32, None, // Process name will be set when we get ProcessName event
        )?;

        self.process_tracks.insert(pid, track_uuid);
        Ok(track_uuid)
    }

    fn ensure_thread_track(&mut self, pid: i32, tid: i32) -> Result<u64> {
        let key = (pid, tid);
        if let Some(&track_uuid) = self.thread_tracks.get(&key) {
            return Ok(track_uuid);
        }

        let process_track_uuid = self.ensure_process_track(pid)?;
        let track_uuid = self.writer.write_thread_descriptor(
            pid as u32,
            tid as u32,
            None,
            process_track_uuid,
        )?;

        self.thread_tracks.insert(key, track_uuid);
        Ok(track_uuid)
    }

    fn convert_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::InternedData(data) => {
                for callstack in &data.callstacks {
                    let mut frames = Vec::new();
                    for &frame_id in callstack.frame_ids.iter() {
                        if let Some(frame) = data.frames.iter().find(|f| f.iid == frame_id) {
                            if let Some(func) = data
                                .function_names
                                .iter()
                                .find(|f| f.iid == frame.function_name_id)
                            {
                                frames.push((func.str.as_ref(), "", frame.rel_pc));
                            }
                        }
                    }

                    let callstack_data = create_callstack_interned_data(callstack.iid, frames);
                    self.writer.write_interned_data(callstack_data)?;
                }
            }
            Event::Sample(sample) => {
                let cpu_mode = match sample.cpu_mode {
                    CpuMode::User => PerfettoCpuMode::ModeUser,
                    CpuMode::Kernel => PerfettoCpuMode::ModeKernel,
                    _ => PerfettoCpuMode::ModeUnknown,
                };

                self.writer.write_perf_sample(
                    sample.cpu,
                    sample.pid as u32,
                    sample.tid as u32,
                    sample.timestamp,
                    sample.callstack_iid,
                    cpu_mode,
                )?;
            }
            Event::Counter(counter) => {
                let thread_track_uuid = self.ensure_thread_track(counter.pid, counter.tid)?;
                let counter_track_key = format!("{}:{}", counter.tid, counter.name);
                let counter_track_uuid =
                    if let Some(&uuid) = self.counter_tracks.get(&counter_track_key) {
                        uuid
                    } else {
                        let uuid = self.writer.write_counter_track(
                            counter.name.to_string(),
                            counter.unit.map(|u| u.to_string()),
                            thread_track_uuid,
                        )?;
                        self.counter_tracks.insert(counter_track_key, uuid);
                        uuid
                    };

                self.writer.write_double_counter_value(
                    counter_track_uuid,
                    counter.value,
                    counter.timestamp,
                )?;
            }
            Event::Span(span) => {
                let track_uuid = self.ensure_thread_track(span.pid, span.tid)?;

                let mut debug_annotations = Vec::new();
                for (key, value) in span.labels.strings.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::String(value.to_string()),
                    ));
                }
                for (key, value) in span.labels.ints.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::Int(*value),
                    ));
                }
                for (key, value) in span.labels.bools.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::Bool(*value),
                    ));
                }
                for (key, value) in span.labels.floats.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::Double(*value),
                    ));
                }

                self.writer.write_slice_begin(
                    track_uuid,
                    span.name.to_string(),
                    span.start_timestamp,
                    debug_annotations,
                )?;
                self.writer
                    .write_slice_end(track_uuid, span.end_timestamp)?;
            }
            Event::Instant(instant) => {
                let track_uuid = self.ensure_thread_track(instant.pid, instant.tid)?;

                let mut debug_annotations = Vec::new();
                for (key, value) in instant.labels.strings.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::String(value.to_string()),
                    ));
                }
                for (key, value) in instant.labels.ints.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::Int(*value),
                    ));
                }
                for (key, value) in instant.labels.bools.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::Bool(*value),
                    ));
                }
                for (key, value) in instant.labels.floats.iter() {
                    debug_annotations.push(create_debug_annotation(
                        key.to_string(),
                        DebugValue::Double(*value),
                    ));
                }

                self.writer.write_instant_event(
                    track_uuid,
                    instant.name.to_string(),
                    instant.timestamp,
                    debug_annotations,
                )?;
            }
            Event::ThreadName(thread_name) => {
                let process_track_uuid = self.ensure_process_track(thread_name.pid)?;
                let _track_uuid = self.ensure_thread_track(thread_name.pid, thread_name.tid)?;
                self.writer.write_thread_descriptor(
                    thread_name.pid as u32,
                    thread_name.tid as u32,
                    Some(thread_name.name.to_string()),
                    process_track_uuid,
                )?;
                debug!(
                    "updated thread name: {} for tid {}",
                    thread_name.name, thread_name.tid
                );
            }
            Event::ProcessName(process_name) => {
                let _track_uuid = self.ensure_process_track(process_name.pid)?;
                self.writer.write_process_descriptor(
                    process_name.pid as u32,
                    Some(process_name.name.to_string()),
                )?;
                debug!(
                    "updated process name: {} for pid {}",
                    process_name.name, process_name.pid
                );
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let bpf_config = BpfConfig::from_file(args.config)?;
    let converter = Rc::new(RefCell::new(ProfilerConverter::new(args.output.clone())?));

    let callback = {
        let converter = converter.clone();
        move |event: Event| {
            if let Err(e) = converter.borrow_mut().convert_event(event) {
                debug!("failed to convert event: {}", e);
            }
        }
    };

    let mut bpf_object = bpf_config.build()?;
    let mut consumer = bpf_object.consumer(callback)?;

    let start_time = std::time::Instant::now();
    let duration = Duration::from_secs(args.duration);
    while start_time.elapsed() < duration {
        consumer.consume()?;
        sleep(Duration::from_millis(10));
    }
    drop(consumer);

    converter.borrow_mut().finish()?;

    Ok(())
}
