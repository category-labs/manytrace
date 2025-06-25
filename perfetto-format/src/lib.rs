use bytes::BytesMut;
use prost::Message;
use protocol::{
    CallstackIterable, FrameIterable, InternedDataIterable, InternedStringIterable, MappingIterable,
};
use std::io::Write;

#[allow(clippy::all)]
#[rustfmt::skip]
pub mod perfetto {
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
}

pub use perfetto::*;

pub struct PerfettoStreamWriter<W: Write> {
    writer: W,
    track_uuid_counter: u64,
}

impl<W: Write> PerfettoStreamWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            track_uuid_counter: 1000,
        }
    }

    fn next_track_uuid(&mut self) -> u64 {
        let uuid = self.track_uuid_counter;
        self.track_uuid_counter += 1;
        uuid
    }

    pub fn write_thread_descriptor(
        &mut self,
        pid: u32,
        tid: u32,
        thread_name: Option<String>,
        parent_uuid: u64,
    ) -> Result<u64, std::io::Error> {
        let thread_track_uuid = self.next_track_uuid();
        self.write_thread_descriptor_with_uuid(
            thread_track_uuid,
            pid,
            tid,
            thread_name,
            parent_uuid,
        )?;
        Ok(thread_track_uuid)
    }

    pub fn write_thread_descriptor_with_uuid(
        &mut self,
        thread_track_uuid: u64,
        pid: u32,
        tid: u32,
        thread_name: Option<String>,
        parent_uuid: u64,
    ) -> Result<(), std::io::Error> {
        let thread_desc = ThreadDescriptor {
            pid: Some(pid as i32),
            tid: Some(tid as i32),
            thread_name,
            ..Default::default()
        };

        let track_desc = TrackDescriptor {
            uuid: Some(thread_track_uuid),
            parent_uuid: Some(parent_uuid),
            thread: Some(thread_desc),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackDescriptor(track_desc)),
            ..Default::default()
        };

        self.write_packet(packet, None)
    }

    pub fn write_slice_begin(
        &mut self,
        track_uuid: u64,
        name: String,
        timestamp_ns: u64,
        debug_annotations: Vec<DebugAnnotation>,
    ) -> Result<(), std::io::Error> {
        let event = TrackEvent {
            name_field: Some(track_event::NameField::Name(name)),
            track_uuid: Some(track_uuid),
            r#type: Some(track_event::Type::SliceBegin as i32),
            debug_annotations,
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackEvent(event)),
            timestamp: Some(timestamp_ns),
            ..Default::default()
        };
        self.write_packet(packet, None)
    }

    pub fn write_slice_end(
        &mut self,
        track_uuid: u64,
        timestamp_ns: u64,
    ) -> Result<(), std::io::Error> {
        let event = TrackEvent {
            track_uuid: Some(track_uuid),
            r#type: Some(track_event::Type::SliceEnd as i32),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackEvent(event)),
            timestamp: Some(timestamp_ns),
            ..Default::default()
        };
        self.write_packet(packet, None)
    }

    pub fn write_packet(
        &mut self,
        mut packet: TracePacket,
        stream_id: Option<u16>,
    ) -> Result<(), std::io::Error> {
        packet.optional_trusted_packet_sequence_id = Some(
            trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                stream_id.map(|id| id as u32).unwrap_or(u32::MAX),
            ),
        );
        let trace = Trace {
            packet: vec![packet],
        };
        let mut buf = BytesMut::new();
        trace.encode(&mut buf).map_err(std::io::Error::other)?;
        self.writer.write_all(&buf)?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), std::io::Error> {
        self.writer.flush()
    }

    pub fn write_protocol_interned_data<'a>(
        &mut self,
        data: &'a impl InternedDataIterable<'a>,
        stream_id: Option<u16>,
    ) -> Result<(), std::io::Error> {
        let mut interned_data = InternedData::default();

        for func in data.function_names() {
            interned_data.function_names.push(InternedString {
                iid: Some(func.iid()),
                str: Some(func.str_ref().as_bytes().to_vec()),
            });
        }

        for frame in data.frames() {
            interned_data.frames.push(Frame {
                iid: Some(frame.iid()),
                function_name_id: Some(frame.function_name_id()),
                mapping_id: Some(frame.mapping_id()),
                rel_pc: Some(frame.rel_pc()),
            });
        }

        for callstack in data.callstacks() {
            interned_data.callstacks.push(Callstack {
                iid: Some(callstack.iid()),
                frame_ids: callstack.frame_ids(),
            });
        }

        for mapping in data.mappings() {
            interned_data.mappings.push(Mapping {
                iid: Some(mapping.iid()),
                build_id: Some(mapping.build_id()),
                exact_offset: Some(mapping.exact_offset()),
                start_offset: Some(mapping.start_offset()),
                start: Some(mapping.start()),
                end: Some(mapping.end()),
                load_bias: Some(mapping.load_bias()),
                path_string_ids: mapping.path_string_ids(),
            });
        }

        for build_id in data.build_ids() {
            interned_data.build_ids.push(InternedString {
                iid: Some(build_id.iid()),
                str: Some(build_id.str_ref().as_bytes().to_vec()),
            });
        }

        if data.continuation() {
            self.write_interned_data_next(interned_data, stream_id)
        } else {
            self.write_interned_data(interned_data, stream_id)
        }
    }

    pub fn write_process_descriptor(
        &mut self,
        pid: u32,
        process_name: Option<String>,
    ) -> Result<u64, std::io::Error> {
        let process_track_uuid = self.next_track_uuid();
        let process_desc = ProcessDescriptor {
            pid: Some(pid as i32),
            process_name,
            ..Default::default()
        };

        let track_desc = TrackDescriptor {
            uuid: Some(process_track_uuid),
            process: Some(process_desc),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackDescriptor(track_desc)),
            ..Default::default()
        };

        self.write_packet(packet, None)?;
        Ok(process_track_uuid)
    }

    pub fn write_interned_data(
        &mut self,
        interned_data: InternedData,
        stream_id: Option<u16>,
    ) -> Result<(), std::io::Error> {
        let packet = TracePacket {
            interned_data: Some(interned_data),
            sequence_flags: Some(trace_packet::SequenceFlags::SeqIncrementalStateCleared as u32),
            ..Default::default()
        };

        self.write_packet(packet, stream_id)
    }

    pub fn write_interned_data_next(
        &mut self,
        interned_data: InternedData,
        stream_id: Option<u16>,
    ) -> Result<(), std::io::Error> {
        let packet = TracePacket {
            interned_data: Some(interned_data),
            sequence_flags: Some(trace_packet::SequenceFlags::SeqNeedsIncrementalState as u32),
            ..Default::default()
        };

        self.write_packet(packet, stream_id)
    }

    pub fn write_instant_event(
        &mut self,
        track_uuid: u64,
        name: String,
        timestamp_ns: u64,
        debug_annotations: Vec<DebugAnnotation>,
    ) -> Result<(), std::io::Error> {
        let event = TrackEvent {
            name_field: Some(track_event::NameField::Name(name)),
            track_uuid: Some(track_uuid),
            r#type: Some(track_event::Type::Instant as i32),
            debug_annotations,
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackEvent(event)),
            timestamp: Some(timestamp_ns),
            ..Default::default()
        };
        self.write_packet(packet, None)
    }

    pub fn write_counter_track(
        &mut self,
        name: String,
        unit_name: Option<String>,
        parent_track_uuid: u64,
    ) -> Result<u64, std::io::Error> {
        let track_uuid = self.next_track_uuid();
        let counter_desc = CounterDescriptor {
            unit_name,
            ..Default::default()
        };

        let track_desc = TrackDescriptor {
            uuid: Some(track_uuid),
            parent_uuid: Some(parent_track_uuid),
            static_or_dynamic_name: Some(track_descriptor::StaticOrDynamicName::Name(name)),
            counter: Some(counter_desc),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackDescriptor(track_desc)),
            ..Default::default()
        };

        self.write_packet(packet, None)?;
        Ok(track_uuid)
    }

    pub fn write_counter_value(
        &mut self,
        track_uuid: u64,
        value: i64,
        timestamp_ns: u64,
    ) -> Result<(), std::io::Error> {
        let event = TrackEvent {
            track_uuid: Some(track_uuid),
            r#type: Some(track_event::Type::Counter as i32),
            counter_value_field: Some(track_event::CounterValueField::CounterValue(value)),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackEvent(event)),
            timestamp: Some(timestamp_ns),
            ..Default::default()
        };

        self.write_packet(packet, None)
    }

    pub fn write_double_counter_value(
        &mut self,
        track_uuid: u64,
        value: f64,
        timestamp_ns: u64,
    ) -> Result<(), std::io::Error> {
        let event = TrackEvent {
            track_uuid: Some(track_uuid),
            r#type: Some(track_event::Type::Counter as i32),
            counter_value_field: Some(track_event::CounterValueField::DoubleCounterValue(value)),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::TrackEvent(event)),
            timestamp: Some(timestamp_ns),
            ..Default::default()
        };

        self.write_packet(packet, None)
    }

    pub fn write_perf_sample(
        &mut self,
        cpu: u32,
        pid: u32,
        tid: u32,
        timestamp_ns: u64,
        callstack_iid: u64,
        stream_id: Option<u16>,
    ) -> Result<(), std::io::Error> {
        let sample = PerfSample {
            cpu: Some(cpu),
            pid: Some(pid),
            tid: Some(tid),
            callstack_iid: Some(callstack_iid),
            timebase_count: Some(1),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::PerfSample(sample)),
            timestamp: Some(timestamp_ns),
            trusted_pid: Some(pid as i32),
            ..Default::default()
        };

        self.write_packet(packet, stream_id)
    }
}

pub fn create_debug_annotation(name: String, value: DebugValue) -> DebugAnnotation {
    DebugAnnotation {
        name_field: Some(debug_annotation::NameField::Name(name)),
        value: Some(match value {
            DebugValue::String(s) => debug_annotation::Value::StringValue(s),
            DebugValue::Int(i) => debug_annotation::Value::IntValue(i),
            DebugValue::Double(d) => debug_annotation::Value::DoubleValue(d),
            DebugValue::Bool(b) => debug_annotation::Value::BoolValue(b),
        }),
        ..Default::default()
    }
}

#[derive(Debug, Clone)]
pub enum DebugValue {
    String(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}
