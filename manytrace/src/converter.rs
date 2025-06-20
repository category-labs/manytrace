use crate::interned_data_iter::{
    CallstackIterable, FrameIterable, InternedDataIterable, InternedStringIterable,
};
use crate::label_iter::LabelIterator;
use perfetto_format::{
    create_callstack_interned_data, create_debug_annotation,
    perfetto::profiling::CpuMode as PerfettoCpuMode, DebugAnnotation, DebugValue,
    PerfettoStreamWriter,
};
use protocol::{ArchivedEvent, CpuMode, Event};
use std::collections::HashMap;
use std::io::Write;

pub struct PerfettoConverter<W: Write> {
    writer: PerfettoStreamWriter<W>,
    process_tracks: HashMap<i32, u64>,
    thread_tracks: HashMap<(i32, i32), u64>,
    counter_tracks: HashMap<String, u64>,
}

impl<W: Write> PerfettoConverter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: PerfettoStreamWriter::new(writer),
            process_tracks: HashMap::new(),
            thread_tracks: HashMap::new(),
            counter_tracks: HashMap::new(),
        }
    }

    fn ensure_process_track(&mut self, pid: i32, name: Option<&str>) -> eyre::Result<u64> {
        if let Some(&track_uuid) = self.process_tracks.get(&pid) {
            return Ok(track_uuid);
        }

        let track_uuid = self.writer.write_process_descriptor(
            pid as u32, name.map(|name| name.to_string()),
        )?;

        self.process_tracks.insert(pid, track_uuid);
        Ok(track_uuid)
    }

    fn ensure_thread_track(&mut self, pid: i32, tid: i32, name: Option<&str>) -> eyre::Result<u64> {
        let key = (pid, tid);
        if let Some(&track_uuid) = self.thread_tracks.get(&key) {
            return Ok(track_uuid);
        }

        let process_track_uuid = self.ensure_process_track(pid, None)?;
        let track_uuid = self.writer.write_thread_descriptor(
            pid as u32,
            tid as u32,
            name.map(|name| name.to_string()),
            process_track_uuid,
        )?;

        self.thread_tracks.insert(key, track_uuid);
        Ok(track_uuid)
    }

    fn create_debug_annotations(labels: &impl LabelIterator) -> Vec<DebugAnnotation> {
        let mut annotations = Vec::new();

        for (key, value) in labels.iter_strings() {
            annotations.push(create_debug_annotation(
                key.to_string(),
                DebugValue::String(value.to_string()),
            ));
        }
        for (key, value) in labels.iter_ints() {
            annotations.push(create_debug_annotation(
                key.to_string(),
                DebugValue::Int(value),
            ));
        }
        for (key, value) in labels.iter_bools() {
            annotations.push(create_debug_annotation(
                key.to_string(),
                DebugValue::Bool(value),
            ));
        }
        for (key, value) in labels.iter_floats() {
            annotations.push(create_debug_annotation(
                key.to_string(),
                DebugValue::Double(value),
            ));
        }

        annotations
    }

    fn convert_interned_data<'a>(
        &mut self,
        data: &'a impl InternedDataIterable<'a>,
    ) -> eyre::Result<()> {
        for callstack in data.callstacks() {
            let frame_ids = callstack.frame_ids();
            let mut frames_with_names = Vec::new();

            for &frame_id in frame_ids.iter() {
                if let Some(frame) = data.frames().find(|f| f.iid() == frame_id) {
                    if let Some(func) = data
                        .function_names()
                        .find(|f| f.iid() == frame.function_name_id())
                    {
                        frames_with_names.push((func.str_ref().to_string(), frame.rel_pc()));
                    }
                }
            }

            let frames_data = frames_with_names
                .iter()
                .map(|(name, rel_pc)| (name.as_str(), "", *rel_pc))
                .collect();

            let callstack_data = create_callstack_interned_data(callstack.iid(), frames_data);
            self.writer.write_interned_data(callstack_data)?;
        }
        Ok(())
    }

    fn convert_counter(
        &mut self,
        pid: i32,
        tid: i32,
        name: &str,
        value: f64,
        timestamp: u64,
        unit: Option<&str>,
    ) -> eyre::Result<()> {
        let thread_track_uuid = self.ensure_thread_track(pid, tid, None)?;
        let counter_track_key = format!("{}:{}", tid, name);
        let counter_track_uuid = if let Some(&uuid) = self.counter_tracks.get(&counter_track_key) {
            uuid
        } else {
            let uuid = self.writer.write_counter_track(
                name.to_string(),
                unit.map(|u| u.to_string()),
                thread_track_uuid,
            )?;
            self.counter_tracks.insert(counter_track_key, uuid);
            uuid
        };

        self.writer
            .write_double_counter_value(counter_track_uuid, value, timestamp)?;
        Ok(())
    }

    fn convert_span(
        &mut self,
        pid: i32,
        tid: i32,
        name: &str,
        start_timestamp: u64,
        end_timestamp: u64,
        labels: &impl LabelIterator,
    ) -> eyre::Result<()> {
        let track_uuid = self.ensure_thread_track(pid, tid, None)?;
        let debug_annotations = Self::create_debug_annotations(labels);

        self.writer.write_slice_begin(
            track_uuid,
            name.to_string(),
            start_timestamp,
            debug_annotations,
        )?;
        self.writer.write_slice_end(track_uuid, end_timestamp)?;
        Ok(())
    }

    fn convert_instant(
        &mut self,
        pid: i32,
        tid: i32,
        name: &str,
        timestamp: u64,
        labels: &impl LabelIterator,
    ) -> eyre::Result<()> {
        let track_uuid = self.ensure_thread_track(pid, tid, None)?;
        let debug_annotations = Self::create_debug_annotations(labels);

        self.writer.write_instant_event(
            track_uuid,
            name.to_string(),
            timestamp,
            debug_annotations,
        )?;
        Ok(())
    }

    fn convert_thread_name(&mut self, pid: i32, tid: i32, name: &str) -> eyre::Result<()> {
        let _track_uuid = self.ensure_thread_track(pid, tid, Some(name))?;
        Ok(())
    }

    fn convert_process_name(&mut self, pid: i32, name: &str) -> eyre::Result<()> {
        let _track_uuid = self.ensure_process_track(pid, Some(name))?;
        Ok(())
    }

    pub fn convert_event(&mut self, event: &Event) -> eyre::Result<()> {
        match event {
            Event::InternedData(data) => self.convert_interned_data(data)?,
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
            Event::Counter(counter) => self.convert_counter(
                counter.pid,
                counter.tid,
                counter.name,
                counter.value,
                counter.timestamp,
                counter.unit,
            )?,
            Event::Span(span) => self.convert_span(
                span.pid,
                span.tid,
                span.name,
                span.start_timestamp,
                span.end_timestamp,
                span.labels.as_ref(),
            )?,
            Event::Instant(instant) => self.convert_instant(
                instant.pid,
                instant.tid,
                instant.name,
                instant.timestamp,
                instant.labels.as_ref(),
            )?,
            Event::ThreadName(thread_name) => {
                self.convert_thread_name(thread_name.pid, thread_name.tid, thread_name.name)?
            }
            Event::ProcessName(process_name) => {
                self.convert_process_name(process_name.pid, process_name.name)?
            }
        }
        Ok(())
    }

    pub fn convert_archived_event<'a>(&mut self, event: &'a ArchivedEvent<'a>) -> eyre::Result<()> {
        match event {
            ArchivedEvent::InternedData(data) => self.convert_interned_data(data)?,
            ArchivedEvent::Sample(sample) => {
                let cpu_mode = match sample.cpu_mode {
                    protocol::ArchivedCpuMode::User => PerfettoCpuMode::ModeUser,
                    protocol::ArchivedCpuMode::Kernel => PerfettoCpuMode::ModeKernel,
                    _ => PerfettoCpuMode::ModeUnknown,
                };

                self.writer.write_perf_sample(
                    sample.cpu.to_native(),
                    sample.pid.to_native() as u32,
                    sample.tid.to_native() as u32,
                    sample.timestamp.to_native(),
                    sample.callstack_iid.to_native(),
                    cpu_mode,
                )?;
            }
            ArchivedEvent::Counter(counter) => self.convert_counter(
                counter.pid.to_native(),
                counter.tid.to_native(),
                counter.name.as_ref(),
                counter.value.to_native(),
                counter.timestamp.to_native(),
                counter.unit.as_ref().map(|u| u.as_ref()),
            )?,
            ArchivedEvent::Span(span) => self.convert_span(
                span.pid.to_native(),
                span.tid.to_native(),
                span.name.as_ref(),
                span.start_timestamp.to_native(),
                span.end_timestamp.to_native(),
                &span.labels,
            )?,
            ArchivedEvent::Instant(instant) => self.convert_instant(
                instant.pid.to_native(),
                instant.tid.to_native(),
                instant.name.as_ref(),
                instant.timestamp.to_native(),
                &instant.labels,
            )?,
            ArchivedEvent::ThreadName(thread_name) => self.convert_thread_name(
                thread_name.pid.to_native(),
                thread_name.tid.to_native(),
                thread_name.name.as_ref(),
            )?,
            ArchivedEvent::ProcessName(process_name) => {
                self.convert_process_name(process_name.pid.to_native(), process_name.name.as_ref())?
            }
        }
        Ok(())
    }

    pub fn flush(&mut self) -> eyre::Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}
