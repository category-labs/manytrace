use bytes::BytesMut;
use prost::Message;
use std::io::Write;

#[allow(clippy::all)]
#[rustfmt::skip]
pub mod perfetto {
    include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
}

pub use perfetto::*;

pub struct PerfettoStreamWriter<W: Write> {
    writer: W,
    sequence_id: u32,
    track_uuid_counter: u64,
    frame_counter: u64,
    function_counter: u64,
}

impl<W: Write> PerfettoStreamWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            sequence_id: 1,
            track_uuid_counter: 1000,
            frame_counter: 0,
            function_counter: 0,
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };

        self.write_packet(packet)
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };
        self.write_packet(packet)
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };
        self.write_packet(packet)
    }

    pub fn write_packet(&mut self, packet: TracePacket) -> Result<(), std::io::Error> {
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

    pub fn write_callstack_interned_data(
        &mut self,
        callstack_iid: u64,
        frames: Vec<(&str, &str, u64)>,
    ) -> Result<(), std::io::Error> {
        let interned_data = if self.frame_counter == 0 && self.function_counter == 0 {
            create_callstack_interned_data(callstack_iid, frames)
        } else {
            create_callstack_interned_next(callstack_iid, self.frame_counter, frames)
        };

        if self.frame_counter == 0 && self.function_counter == 0 {
            self.write_interned_data(interned_data)
        } else {
            self.write_interned_data_next(interned_data)
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };

        self.write_packet(packet)?;
        Ok(process_track_uuid)
    }

    pub fn write_interned_data(
        &mut self,
        interned_data: InternedData,
    ) -> Result<(), std::io::Error> {
        self.frame_counter += interned_data.frames.len() as u64;
        self.function_counter += interned_data.function_names.len() as u64;

        let packet = TracePacket {
            interned_data: Some(interned_data),
            sequence_flags: Some(trace_packet::SequenceFlags::SeqIncrementalStateCleared as u32),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };
        self.write_packet(packet)
    }

    pub fn write_interned_data_next(
        &mut self,
        interned_data: InternedData,
    ) -> Result<(), std::io::Error> {
        self.frame_counter += interned_data.frames.len() as u64;
        self.function_counter += interned_data.function_names.len() as u64;

        let packet = TracePacket {
            interned_data: Some(interned_data),
            sequence_flags: Some(trace_packet::SequenceFlags::SeqNeedsIncrementalState as u32),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };
        self.write_packet(packet)
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };
        self.write_packet(packet)
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };

        self.write_packet(packet)?;
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };

        self.write_packet(packet)
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
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };

        self.write_packet(packet)
    }

    pub fn write_perf_sample(
        &mut self,
        cpu: u32,
        pid: u32,
        tid: u32,
        timestamp_ns: u64,
        callstack_iid: u64,
        cpu_mode: profiling::CpuMode,
    ) -> Result<(), std::io::Error> {
        let sample = PerfSample {
            cpu: Some(cpu),
            pid: Some(pid),
            tid: Some(tid),
            callstack_iid: Some(callstack_iid),
            cpu_mode: Some(cpu_mode as i32),
            timebase_count: Some(1),
            ..Default::default()
        };

        let packet = TracePacket {
            data: Some(trace_packet::Data::PerfSample(sample)),
            timestamp: Some(timestamp_ns),
            trusted_pid: Some(pid as i32),
            optional_trusted_packet_sequence_id: Some(
                trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    self.sequence_id,
                ),
            ),
            ..Default::default()
        };
        self.write_packet(packet)
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

pub fn create_callstack_interned_data(
    callstack_iid: u64,
    frames: Vec<(&str, &str, u64)>,
) -> InternedData {
    let mut interned_data = InternedData::default();

    interned_data.function_names.push(InternedString {
        iid: Some(0),
        str: Some(b"<unknown>".to_vec()),
    });

    for (i, (function_name, _, _)) in frames.iter().enumerate() {
        let function = InternedString {
            iid: Some(1 + i as u64),
            str: Some(function_name.as_bytes().to_vec()),
        };
        interned_data.function_names.push(function);
    }

    for (i, (_, _, addr)) in frames.iter().enumerate() {
        let frame = Frame {
            iid: Some(i as u64),
            function_name_id: Some(1 + i as u64),
            mapping_id: Some(1),
            rel_pc: Some(*addr),
        };
        interned_data.frames.push(frame);
    }

    let callstack = Callstack {
        iid: Some(callstack_iid),
        frame_ids: frames.iter().enumerate().map(|(i, _)| i as u64).collect(),
    };
    interned_data.mappings.push(Mapping {
        iid: Some(1),
        build_id: Some(1),
        exact_offset: Some(0),
        start_offset: Some(0),
        start: Some(0),
        end: Some(0x7fffffffffffffff),
        load_bias: Some(0),
        path_string_ids: vec![],
    });

    interned_data.build_ids.push(InternedString {
        iid: Some(1),
        str: Some(b"unknown".to_vec()),
    });

    interned_data.callstacks.push(callstack);
    interned_data
}

pub fn create_callstack_interned_next(
    callstack_iid: u64,
    frame_idx_start: u64,
    frames: Vec<(&str, &str, u64)>,
) -> InternedData {
    let mut interned_data = InternedData::default();

    let function_idx_start = frame_idx_start + 1; // +1 for the initial <unknown> function

    for (i, (function_name, _, _)) in frames.iter().enumerate() {
        let function = InternedString {
            iid: Some(function_idx_start + i as u64),
            str: Some(function_name.as_bytes().to_vec()),
        };
        interned_data.function_names.push(function);
    }

    for (i, (_, _, addr)) in frames.iter().enumerate() {
        let frame = Frame {
            iid: Some(frame_idx_start + i as u64),
            function_name_id: Some(function_idx_start + i as u64),
            mapping_id: Some(1),
            rel_pc: Some(*addr),
        };
        interned_data.frames.push(frame);
    }

    let callstack = Callstack {
        iid: Some(callstack_iid),
        frame_ids: interned_data.frames.iter().map(|fr| fr.iid()).collect(),
    };

    interned_data.callstacks.push(callstack);
    interned_data
}
