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

use crate::label_iter::LabelIterator;
use perfetto_format::{create_debug_annotation, DebugAnnotation, DebugValue, PerfettoStreamWriter};
use protocol::{ArchivedEvent, Event, InternedDataIterable};
use std::collections::HashMap;
use std::io::Write;

pub struct PerfettoConverter<W: Write> {
    writer: PerfettoStreamWriter<W>,
    track_map: HashMap<protocol::TrackId, u64>,
}

impl<W: Write> PerfettoConverter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: PerfettoStreamWriter::new(writer),
            track_map: HashMap::new(),
        }
    }

    fn ensure_process_track(&mut self, pid: i32, name: Option<&str>) -> eyre::Result<u64> {
        let track_id = protocol::TrackId::Process { pid };
        if let Some(&track_uuid) = self.track_map.get(&track_id) {
            return Ok(track_uuid);
        }

        let track_uuid = self
            .writer
            .write_process_descriptor(pid as u32, name.map(|name| name.to_string()))?;

        self.track_map.insert(track_id, track_uuid);
        Ok(track_uuid)
    }

    fn ensure_thread_track(&mut self, pid: i32, tid: i32, name: Option<&str>) -> eyre::Result<u64> {
        let track_id = protocol::TrackId::Thread { tid, pid };

        match self.track_map.get(&track_id).copied() {
            Some(track_uuid) => {
                if name.is_some() {
                    let process_track_uuid = self.ensure_process_track(pid, None)?;
                    self.writer.write_thread_descriptor_with_uuid(
                        track_uuid,
                        pid as u32,
                        tid as u32,
                        name.map(|n| n.to_string()),
                        process_track_uuid,
                    )?;
                }
                Ok(track_uuid)
            }
            None => {
                let process_track_uuid = self.ensure_process_track(pid, None)?;
                let track_uuid = self.writer.write_thread_descriptor(
                    pid as u32,
                    tid as u32,
                    name.map(|n| n.to_string()),
                    process_track_uuid,
                )?;
                self.track_map.insert(track_id, track_uuid);
                Ok(track_uuid)
            }
        }
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
        stream_id: Option<protocol::StreamId>,
    ) -> eyre::Result<()> {
        self.writer.write_protocol_interned_data(data, stream_id)?;
        Ok(())
    }

    fn convert_counter(&mut self, counter: &protocol::Counter) -> eyre::Result<()> {
        if let Some(&track_uuid) = self.track_map.get(&counter.track_id) {
            self.writer
                .write_double_counter_value(track_uuid, counter.value, counter.timestamp)?;
        }
        Ok(())
    }

    fn convert_archived_counter(
        &mut self,
        counter: &protocol::ArchivedCounter,
    ) -> eyre::Result<()> {
        let track_id = counter.track_id.to_native();

        if let Some(&track_uuid) = self.track_map.get(&track_id) {
            self.writer.write_double_counter_value(
                track_uuid,
                counter.value.to_native(),
                counter.timestamp.to_native(),
            )?;
        }
        Ok(())
    }

    fn convert_span(
        &mut self,
        track_id: &protocol::TrackId,
        name: &str,
        start_timestamp: u64,
        end_timestamp: u64,
        labels: &impl LabelIterator,
    ) -> eyre::Result<()> {
        let track_uuid = if let Some(&uuid) = self.track_map.get(track_id) {
            uuid
        } else {
            match track_id {
                protocol::TrackId::Thread { tid, pid } => {
                    let uuid = self.ensure_thread_track(*pid, *tid, None)?;
                    self.track_map.insert(*track_id, uuid);
                    uuid
                }
                protocol::TrackId::Process { pid } => {
                    let uuid = self.ensure_process_track(*pid, None)?;
                    self.track_map.insert(*track_id, uuid);
                    uuid
                }
                _ => return Ok(()),
            }
        };
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
        track_id: &protocol::TrackId,
        name: &str,
        timestamp: u64,
        labels: &impl LabelIterator,
    ) -> eyre::Result<()> {
        let track_uuid = if let Some(&uuid) = self.track_map.get(track_id) {
            uuid
        } else {
            match track_id {
                protocol::TrackId::Thread { tid, pid } => {
                    let uuid = self.ensure_thread_track(*pid, *tid, None)?;
                    self.track_map.insert(*track_id, uuid);
                    uuid
                }
                protocol::TrackId::Process { pid } => {
                    let uuid = self.ensure_process_track(*pid, None)?;
                    self.track_map.insert(*track_id, uuid);
                    uuid
                }
                _ => return Ok(()),
            }
        };
        let debug_annotations = Self::create_debug_annotations(labels);

        self.writer.write_instant_event(
            track_uuid,
            name.to_string(),
            timestamp,
            debug_annotations,
        )?;
        Ok(())
    }

    fn convert_track(&mut self, track: &protocol::Track) -> eyre::Result<()> {
        use protocol::TrackType;

        let track_id = track.track_type.to_id();
        if self.track_map.contains_key(&track_id) {
            return Ok(());
        }

        let parent_uuid = if let Some(parent) = &track.parent {
            let parent_id = parent.to_id();
            if !self.track_map.contains_key(&parent_id) {
                match parent {
                    TrackType::Thread { tid, pid } => {
                        let uuid = self.ensure_thread_track(*pid, *tid, None)?;
                        self.track_map.insert(parent_id, uuid);
                        uuid
                    }
                    TrackType::Process { pid } => {
                        let uuid = self.ensure_process_track(*pid, None)?;
                        self.track_map.insert(parent_id, uuid);
                        uuid
                    }
                    TrackType::Cpu { .. }
                    | TrackType::Custom { .. }
                    | TrackType::Counter { .. } => *self.track_map.get(&parent_id).unwrap_or(&0),
                }
            } else {
                *self.track_map.get(&parent_id).unwrap()
            }
        } else {
            0
        };

        let track_uuid = match &track.track_type {
            TrackType::Cpu { .. } | TrackType::Custom { .. } => self
                .writer
                .write_generic_track(track.name.to_string(), parent_uuid)?,
            TrackType::Counter { unit, .. } => self.writer.write_counter_track(
                track.name.to_string(),
                unit.map(|u| u.to_string()),
                parent_uuid,
            )?,
            TrackType::Thread { tid, pid } => {
                self.ensure_thread_track(*pid, *tid, Some(track.name))?
            }
            TrackType::Process { pid } => self.ensure_process_track(*pid, Some(track.name))?,
        };

        self.track_map.insert(track_id, track_uuid);
        Ok(())
    }

    pub fn convert_message(&mut self, message: &protocol::Message) -> eyre::Result<()> {
        let (event, stream_id) = match message {
            protocol::Message::Event(e) => (e, None),
            protocol::Message::Stream { stream_id, event } => (event, Some(*stream_id)),
        };
        self.convert_event(event, stream_id)
    }

    fn convert_event(
        &mut self,
        event: &Event,
        stream_id: Option<protocol::StreamId>,
    ) -> eyre::Result<()> {
        match event {
            Event::InternedData(data) => self.convert_interned_data(data, stream_id)?,
            Event::Sample(sample) => {
                if let protocol::TrackId::Thread { tid, pid } = sample.track_id {
                    self.writer.write_perf_sample(
                        sample.cpu,
                        pid as u32,
                        tid as u32,
                        sample.timestamp,
                        sample.callstack_iid,
                        stream_id,
                    )?;
                }
            }
            Event::Counter(counter) => self.convert_counter(counter)?,
            Event::Span(span) => self.convert_span(
                &span.track_id,
                span.name,
                span.start_timestamp,
                span.end_timestamp,
                span.labels.as_ref(),
            )?,
            Event::Instant(instant) => self.convert_instant(
                &instant.track_id,
                instant.name,
                instant.timestamp,
                instant.labels.as_ref(),
            )?,
            Event::Track(track) => self.convert_track(track)?,
        }
        Ok(())
    }

    pub fn convert_archived_message<'a>(
        &mut self,
        message: &'a protocol::ArchivedMessage<'a>,
    ) -> eyre::Result<()> {
        let (event, stream_id) = match message {
            protocol::ArchivedMessage::Event(e) => (e, None),
            protocol::ArchivedMessage::Stream { stream_id, event } => {
                (event, Some(stream_id.to_native()))
            }
        };
        self.convert_archived_event(event, stream_id)
    }

    pub fn convert_archived_event<'a>(
        &mut self,
        event: &'a ArchivedEvent<'a>,
        stream_id: Option<protocol::StreamId>,
    ) -> eyre::Result<()> {
        match event {
            ArchivedEvent::InternedData(data) => self.convert_interned_data(data, stream_id)?,
            ArchivedEvent::Sample(sample) => {
                if let protocol::ArchivedTrackId::Thread { tid, pid } = &sample.track_id {
                    self.writer.write_perf_sample(
                        sample.cpu.to_native(),
                        pid.to_native() as u32,
                        tid.to_native() as u32,
                        sample.timestamp.to_native(),
                        sample.callstack_iid.to_native(),
                        stream_id,
                    )?;
                }
            }
            ArchivedEvent::Counter(counter) => self.convert_archived_counter(counter)?,
            ArchivedEvent::Span(span) => {
                let track_id = span.track_id.to_native();
                self.convert_span(
                    &track_id,
                    span.name.as_ref(),
                    span.start_timestamp.to_native(),
                    span.end_timestamp.to_native(),
                    &span.labels,
                )?
            }
            ArchivedEvent::Instant(instant) => {
                let track_id = instant.track_id.to_native();
                self.convert_instant(
                    &track_id,
                    instant.name.as_ref(),
                    instant.timestamp.to_native(),
                    &instant.labels,
                )?
            }
            ArchivedEvent::Track(track) => {
                let track_type = track.track_type.to_native();
                let parent = track.parent.as_ref().map(|p| p.to_native());

                let owned_track = protocol::Track {
                    name: track.name.as_ref(),
                    track_type,
                    parent,
                };

                self.convert_track(&owned_track)?
            }
        }
        Ok(())
    }

    pub fn flush(&mut self) -> eyre::Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}
