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

mod blocktrack_skel {
    include!(concat!(env!("OUT_DIR"), "/blocktrack.skel.rs"));
}

use blocktrack_skel::*;

use crate::{get_monotonic_timestamp, BpfError};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, OpenObject, RingBufferBuilder};
use nix::sys::stat::{major, minor};
use protocol::{Counter, Event, Labels, Message, Span, Track, TrackId, TrackType};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::mem::MaybeUninit;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tracing::debug;

const OPERATION_COUNT: usize = 5;
const HISTOGRAM_BUCKETS: usize = 32;
const MAP_BATCH_SIZE: u32 = 64;
const MIN_RINGBUF_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockOperation {
    Read,
    Write,
    Flush,
    Discard,
    Other,
}

impl BlockOperation {
    fn index(self) -> usize {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Flush => 2,
            Self::Discard => 3,
            Self::Other => 4,
        }
    }

    fn from_index(index: u8) -> Option<Self> {
        match index as usize {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Flush),
            3 => Some(Self::Discard),
            4 => Some(Self::Other),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Flush => "flush",
            Self::Discard => "discard",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockMetric {
    Throughput,
    Iops,
    Latency,
    Saturation,
    Errors,
}

fn default_frequency() -> u64 {
    9
}

fn default_operations() -> Vec<BlockOperation> {
    vec![BlockOperation::Read, BlockOperation::Write]
}

fn default_metrics() -> Vec<BlockMetric> {
    vec![
        BlockMetric::Throughput,
        BlockMetric::Iops,
        BlockMetric::Latency,
        BlockMetric::Saturation,
        BlockMetric::Errors,
    ]
}

fn default_max_devices() -> u32 {
    64
}

fn default_max_requests() -> u32 {
    32768
}

fn default_ringbuf_size() -> usize {
    8 * 1024 * 1024
}

fn default_timeline_sample_every() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockIoConfig {
    #[serde(default = "default_frequency")]
    pub frequency: u64,
    #[serde(default = "default_operations")]
    pub operations: Vec<BlockOperation>,
    #[serde(default = "default_metrics")]
    pub metrics: Vec<BlockMetric>,
    /// Device names, /dev paths, or major:minor values. Partitions resolve to
    /// their physical parent because request timelines are per hardware queue.
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default = "default_max_devices")]
    pub max_devices: u32,
    #[serde(default = "default_max_requests")]
    pub max_requests: u32,
    #[serde(default)]
    pub timeline: bool,
    #[serde(default = "default_timeline_sample_every")]
    pub timeline_sample_every: u32,
    #[serde(default)]
    pub timeline_min_latency_us: u64,
    #[serde(default = "default_ringbuf_size")]
    pub ringbuf: usize,
}

impl Default for BlockIoConfig {
    fn default() -> Self {
        Self {
            frequency: default_frequency(),
            operations: default_operations(),
            metrics: default_metrics(),
            devices: Vec::new(),
            max_devices: default_max_devices(),
            max_requests: default_max_requests(),
            timeline: false,
            timeline_sample_every: default_timeline_sample_every(),
            timeline_min_latency_us: 0,
            ringbuf: default_ringbuf_size(),
        }
    }
}

impl BlockIoConfig {
    fn has_metric(&self, metric: BlockMetric) -> bool {
        self.metrics.contains(&metric)
    }

    fn operation_mask(&self) -> u32 {
        self.operations
            .iter()
            .fold(0, |mask, operation| mask | (1 << operation.index()))
    }

    fn validate(&self) -> Result<(), BpfError> {
        if self.frequency == 0 {
            return Err(BpfError::LoadError(
                "block_io frequency must be greater than zero".to_string(),
            ));
        }
        if self.operations.is_empty() || self.metrics.is_empty() {
            return Err(BpfError::LoadError(
                "block_io operations and metrics must not be empty".to_string(),
            ));
        }
        if self.max_devices == 0 || self.max_requests == 0 {
            return Err(BpfError::LoadError(
                "block_io map limits must be greater than zero".to_string(),
            ));
        }
        if self.timeline_sample_every == 0 {
            return Err(BpfError::LoadError(
                "block_io timeline_sample_every must be greater than zero".to_string(),
            ));
        }
        if self.timeline
            && (self.ringbuf < MIN_RINGBUF_SIZE
                || self.ringbuf > u32::MAX as usize
                || !self.ringbuf.is_power_of_two())
        {
            return Err(BpfError::LoadError(
                "block_io ringbuf must be a power of two between 4096 and 4294967295 bytes"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

pub struct Object {
    object: MaybeUninit<OpenObject>,
    config: BlockIoConfig,
}

impl Object {
    pub fn new(config: BlockIoConfig) -> Self {
        Self {
            object: MaybeUninit::uninit(),
            config,
        }
    }

    pub fn build<'bd, F>(&'bd mut self, callback: F) -> Result<BlockIo<'bd, F>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) -> i32 + 'bd,
    {
        self.config.validate()?;
        BlockIo::new(&mut self.object, self.config.clone(), callback)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OperationStats {
    bytes: u64,
    operations: u64,
    errors: u64,
    queue_latency_ns: u64,
    service_latency_ns: u64,
    total_latency_ns: u64,
    service_histogram: [u64; HISTOGRAM_BUCKETS],
    total_histogram: [u64; HISTOGRAM_BUCKETS],
}

impl Default for OperationStats {
    fn default() -> Self {
        Self {
            bytes: 0,
            operations: 0,
            errors: 0,
            queue_latency_ns: 0,
            service_latency_ns: 0,
            total_latency_ns: 0,
            service_histogram: [0; HISTOGRAM_BUCKETS],
            total_histogram: [0; HISTOGRAM_BUCKETS],
        }
    }
}

unsafe impl plain::Plain for OperationStats {}

impl OperationStats {
    fn delta(self, previous: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_sub(previous.bytes),
            operations: self.operations.saturating_sub(previous.operations),
            errors: self.errors.saturating_sub(previous.errors),
            queue_latency_ns: self
                .queue_latency_ns
                .saturating_sub(previous.queue_latency_ns),
            service_latency_ns: self
                .service_latency_ns
                .saturating_sub(previous.service_latency_ns),
            total_latency_ns: self
                .total_latency_ns
                .saturating_sub(previous.total_latency_ns),
            service_histogram: std::array::from_fn(|index| {
                self.service_histogram[index].saturating_sub(previous.service_histogram[index])
            }),
            total_histogram: std::array::from_fn(|index| {
                self.total_histogram[index].saturating_sub(previous.total_histogram[index])
            }),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeviceStats {
    operations: [OperationStats; OPERATION_COUNT],
    requeues: u64,
    lost_requests: u64,
    timeline_drops: u64,
    first_seen_ns: u64,
    last_seen_ns: u64,
}

impl Default for DeviceStats {
    fn default() -> Self {
        Self {
            operations: [OperationStats::default(); OPERATION_COUNT],
            requeues: 0,
            lost_requests: 0,
            timeline_drops: 0,
            first_seen_ns: 0,
            last_seen_ns: 0,
        }
    }
}

unsafe impl plain::Plain for DeviceStats {}

impl DeviceStats {
    fn delta(self, previous: Option<Self>) -> Self {
        let Some(previous) = previous.filter(|value| value.first_seen_ns == self.first_seen_ns)
        else {
            return self;
        };
        Self {
            operations: std::array::from_fn(|index| {
                self.operations[index].delta(previous.operations[index])
            }),
            requeues: self.requeues.saturating_sub(previous.requeues),
            lost_requests: self.lost_requests.saturating_sub(previous.lost_requests),
            timeline_drops: self.timeline_drops.saturating_sub(previous.timeline_drops),
            first_seen_ns: self.first_seen_ns,
            last_seen_ns: self.last_seen_ns,
        }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DeviceDepth {
    lock: u32,
    padding: u32,
    first_seen_ns: u64,
    last_update_ns: u64,
    busy_ns: u64,
    saturated_ns: u64,
    inflight_ns: u64,
    queued_ns: u64,
    inflight: u32,
    queued: u32,
    max_inflight: u32,
    max_queued: u32,
}

unsafe impl plain::Plain for DeviceDepth {}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DepthSnapshot {
    first_seen_ns: u64,
    busy_ns: u64,
    saturated_ns: u64,
    inflight_ns: u64,
    queued_ns: u64,
    inflight: u32,
    queued: u32,
    max_inflight: u32,
    max_queued: u32,
}

impl DeviceDepth {
    fn snapshot(self, now: u64) -> DepthSnapshot {
        let elapsed = now.saturating_sub(self.last_update_ns);
        DepthSnapshot {
            first_seen_ns: self.first_seen_ns,
            busy_ns: self
                .busy_ns
                .saturating_add(if self.inflight > 0 { elapsed } else { 0 }),
            saturated_ns: self.saturated_ns.saturating_add(if self.queued > 0 {
                elapsed
            } else {
                0
            }),
            inflight_ns: self
                .inflight_ns
                .saturating_add(elapsed.saturating_mul(self.inflight as u64)),
            queued_ns: self
                .queued_ns
                .saturating_add(elapsed.saturating_mul(self.queued as u64)),
            inflight: self.inflight,
            queued: self.queued,
            max_inflight: self.max_inflight,
            max_queued: self.max_queued,
        }
    }
}

impl DepthSnapshot {
    fn delta(self, previous: Option<Self>) -> Self {
        let Some(previous) = previous.filter(|value| value.first_seen_ns == self.first_seen_ns)
        else {
            return self;
        };
        Self {
            first_seen_ns: self.first_seen_ns,
            busy_ns: self.busy_ns.saturating_sub(previous.busy_ns),
            saturated_ns: self.saturated_ns.saturating_sub(previous.saturated_ns),
            inflight_ns: self.inflight_ns.saturating_sub(previous.inflight_ns),
            queued_ns: self.queued_ns.saturating_sub(previous.queued_ns),
            inflight: self.inflight,
            queued: self.queued,
            max_inflight: self.max_inflight,
            max_queued: self.max_queued,
        }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RequestSpanEvent {
    request_id: u64,
    start_ns: u64,
    end_ns: u64,
    queue_ns: u64,
    service_ns: u64,
    total_ns: u64,
    sector: u64,
    dev: u32,
    partition_dev: u32,
    bytes: u32,
    tgid: u32,
    pid: u32,
    requeues: u32,
    error: i32,
    operation: u8,
    completed: u8,
    padding: [u8; 6],
    comm: [u8; 16],
}

unsafe impl plain::Plain for RequestSpanEvent {}

impl RequestSpanEvent {
    fn comm(&self) -> String {
        let end = self
            .comm
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.comm.len());
        String::from_utf8_lossy(&self.comm[..end]).into_owned()
    }
}

#[derive(Debug, Clone, Default)]
struct MountInfo {
    filesystem: String,
    source: String,
    mountpoint: String,
}

#[derive(Debug, Clone)]
struct DeviceInfo {
    dev: u32,
    name: String,
    mounts: Vec<MountInfo>,
}

impl DeviceInfo {
    fn major_minor(&self) -> String {
        let (major, minor) = decode_device(self.dev);
        format!("{major}:{minor}")
    }

    fn track_name(&self) -> String {
        let mut name = format!("{} ({})", self.name, self.major_minor());
        if !self.mounts.is_empty() {
            let mut filesystems = self
                .mounts
                .iter()
                .map(|mount| mount.filesystem.as_str())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            filesystems.sort_unstable();
            let filesystems = filesystems.join(",");
            let mountpoints = self
                .mounts
                .iter()
                .take(3)
                .map(|mount| mount.mountpoint.as_str())
                .collect::<Vec<_>>()
                .join(",");
            name.push_str(&format!(" {filesystems} {mountpoints}"));
        }
        name
    }
}

struct DeviceRegistry {
    mounts: HashMap<u32, Vec<MountInfo>>,
    infos: HashMap<u32, DeviceInfo>,
    track_ids: HashMap<u32, u64>,
}

impl DeviceRegistry {
    fn new() -> Self {
        let mut mounts = read_mountinfo().unwrap_or_default();
        for (mounted_dev, device_mounts) in mounts.clone() {
            if let Ok(parent) = physical_device(mounted_dev) {
                if parent != mounted_dev {
                    mounts.entry(parent).or_default().extend(device_mounts);
                }
            }
        }
        Self {
            mounts,
            infos: HashMap::new(),
            track_ids: HashMap::new(),
        }
    }

    fn info(&mut self, dev: u32) -> DeviceInfo {
        if let Some(info) = self.infos.get(&dev) {
            return info.clone();
        }
        let (major, minor) = decode_device(dev);
        let sysfs = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
        let name = fs::canonicalize(&sysfs)
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| format!("block-{major}:{minor}"));
        let info = DeviceInfo {
            dev,
            name,
            mounts: self.mounts.get(&dev).cloned().unwrap_or_default(),
        };
        self.infos.insert(dev, info.clone());
        info
    }

    fn ensure_track<F>(&mut self, dev: u32, callback: &Rc<RefCell<F>>) -> (u64, i32)
    where
        F: for<'a> FnMut(Message<'a>) -> i32,
    {
        if let Some(id) = self.track_ids.get(&dev) {
            return (*id, 0);
        }
        let id = rand::thread_rng().gen::<u64>();
        let info = self.info(dev);
        let name = info.track_name();
        let result = callback.borrow_mut()(Message::Event(Event::Track(Track {
            name: &name,
            track_type: TrackType::Custom { id },
            parent: None,
        })));
        if result == 0 {
            self.track_ids.insert(dev, id);
        }
        (id, result)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CounterKind {
    Throughput,
    Iops,
    QueueAverage,
    ServiceAverage,
    TotalAverage,
    ServiceP50,
    ServiceP95,
    ServiceP99,
    TotalP50,
    TotalP95,
    TotalP99,
    Errors,
    BusyPercent,
    SaturatedPercent,
    AverageInflight,
    AverageQueued,
    Inflight,
    Queued,
    MaxInflight,
    MaxQueued,
    Requeues,
    LostRequests,
    TimelineDrops,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SeriesKey {
    dev: u32,
    operation: Option<BlockOperation>,
    kind: CounterKind,
}

struct CounterSample {
    operation: Option<BlockOperation>,
    kind: CounterKind,
    name: String,
    value: f64,
    unit: &'static str,
}

pub struct BlockIo<'this, F> {
    skel: BlocktrackSkel<'this>,
    ringbuf: Option<libbpf_rs::RingBuffer<'this>>,
    callback: Rc<RefCell<F>>,
    registry: Rc<RefCell<DeviceRegistry>>,
    config: BlockIoConfig,
    last_sample_ns: u64,
    previous_stats: HashMap<u32, DeviceStats>,
    previous_depth: HashMap<u32, DepthSnapshot>,
    series_ids: HashMap<SeriesKey, u64>,
}

impl<'this, F> BlockIo<'this, F>
where
    F: for<'a> FnMut(Message<'a>) -> i32 + 'this,
{
    fn new(
        open_object: &'this mut MaybeUninit<OpenObject>,
        config: BlockIoConfig,
        callback: F,
    ) -> Result<Self, BpfError> {
        let device_filters = config
            .devices
            .iter()
            .map(|device| resolve_device_filter(device))
            .collect::<Result<HashSet<_>, _>>()?;
        if device_filters.len() > config.max_devices as usize {
            return Err(BpfError::LoadError(format!(
                "block_io has {} device filters but max_devices is {}",
                device_filters.len(),
                config.max_devices
            )));
        }
        let depth_devices = if config.has_metric(BlockMetric::Saturation) {
            if device_filters.is_empty() {
                discover_physical_devices()?
            } else {
                device_filters.clone()
            }
        } else {
            HashSet::new()
        };
        if depth_devices.len() > config.max_devices as usize {
            return Err(BpfError::LoadError(format!(
                "block_io found {} devices but max_devices is {}; increase max_devices or configure devices",
                depth_devices.len(), config.max_devices
            )));
        }
        let request_tracking = config.has_metric(BlockMetric::Iops)
            || config.has_metric(BlockMetric::Latency)
            || config.has_metric(BlockMetric::Saturation)
            || config.timeline;

        let skel_builder = BlocktrackSkelBuilder::default();
        let mut open_skel = skel_builder.open(open_object).map_err(|error| {
            BpfError::LoadError(format!("failed to open block_io BPF: {error}"))
        })?;
        open_skel
            .maps
            .device_stats
            .set_max_entries(config.max_devices)
            .map_err(|error| {
                BpfError::LoadError(format!("failed to size block_io device map: {error}"))
            })?;
        open_skel
            .maps
            .device_depths
            .set_max_entries(if config.has_metric(BlockMetric::Saturation) {
                config.max_devices
            } else {
                1
            })
            .map_err(|error| {
                BpfError::LoadError(format!("failed to size block_io depth map: {error}"))
            })?;
        open_skel
            .maps
            .requests
            .set_max_entries(if request_tracking {
                config.max_requests
            } else {
                1
            })
            .map_err(|error| {
                BpfError::LoadError(format!("failed to size block_io request map: {error}"))
            })?;
        open_skel
            .maps
            .tracked_devices
            .set_max_entries(config.max_devices.max(device_filters.len() as u32).max(1))
            .map_err(|error| {
                BpfError::LoadError(format!("failed to size block_io filter map: {error}"))
            })?;
        open_skel
            .maps
            .events
            .set_max_entries(if config.timeline {
                config.ringbuf as u32
            } else {
                MIN_RINGBUF_SIZE as u32
            })
            .map_err(|error| {
                BpfError::LoadError(format!("failed to size block_io ring buffer: {error}"))
            })?;

        let rodata = open_skel.maps.rodata_data.as_mut().unwrap();
        rodata.cfg.operation_mask = config.operation_mask();
        rodata.cfg.timeline_sample_every = config.timeline_sample_every;
        rodata.cfg.timeline_min_latency_ns = config.timeline_min_latency_us.saturating_mul(1000);
        rodata
            .cfg
            .bytes_enabled
            .write(config.has_metric(BlockMetric::Throughput));
        // Latency averages need a completion denominator even when the IOPS
        // series itself is disabled.
        rodata
            .cfg
            .operations_enabled
            .write(config.has_metric(BlockMetric::Iops) || config.has_metric(BlockMetric::Latency));
        rodata
            .cfg
            .latency_enabled
            .write(config.has_metric(BlockMetric::Latency));
        rodata
            .cfg
            .saturation_enabled
            .write(config.has_metric(BlockMetric::Saturation));
        rodata
            .cfg
            .errors_enabled
            .write(config.has_metric(BlockMetric::Errors));
        rodata.cfg.timeline_enabled.write(config.timeline);
        rodata.cfg.filter_enabled.write(!device_filters.is_empty());
        rodata.cfg.request_tracking_enabled.write(request_tracking);

        let mut skel = open_skel.load().map_err(|error| {
            BpfError::LoadError(format!("failed to load block_io BPF: {error}"))
        })?;
        let initialized_at = get_monotonic_timestamp();
        let initial_depth = DeviceDepth {
            first_seen_ns: initialized_at,
            last_update_ns: initialized_at,
            ..DeviceDepth::default()
        };
        let initial_depth = unsafe { plain::as_bytes(&initial_depth) };
        for dev in depth_devices {
            skel.maps
                .device_depths
                .update(&dev.to_ne_bytes(), initial_depth, MapFlags::NO_EXIST)
                .map_err(|error| {
                    BpfError::MapError(format!(
                        "failed to initialize block_io depth for {}: {error}",
                        format_device(dev)
                    ))
                })?;
        }
        for dev in device_filters {
            skel.maps
                .tracked_devices
                .update(&dev.to_ne_bytes(), &1u32.to_ne_bytes(), MapFlags::ANY)
                .map_err(|error| {
                    BpfError::MapError(format!("failed to add block_io device filter: {error}"))
                })?;
        }
        skel.attach().map_err(|error| {
            BpfError::AttachError(format!("failed to attach block_io programs: {error}"))
        })?;

        let callback = Rc::new(RefCell::new(callback));
        let registry = Rc::new(RefCell::new(DeviceRegistry::new()));
        let ringbuf = if config.timeline {
            let callback_ref = callback.clone();
            let registry_ref = registry.clone();
            let mut builder = RingBufferBuilder::new();
            builder
                .add(&skel.maps.events, move |data| {
                    let event = match plain::from_bytes::<RequestSpanEvent>(data) {
                        Ok(event) => *event,
                        Err(error) => {
                            debug!(?error, "failed to parse block request span");
                            return 0;
                        }
                    };
                    emit_request_span(&callback_ref, &registry_ref, event)
                })
                .map_err(|error| {
                    BpfError::MapError(format!("failed to add block_io ring buffer: {error}"))
                })?;
            Some(builder.build().map_err(|error| {
                BpfError::MapError(format!("failed to build block_io ring buffer: {error}"))
            })?)
        } else {
            None
        };

        debug!(
            frequency = config.frequency,
            operations = ?config.operations,
            metrics = ?config.metrics,
            timeline = config.timeline,
            devices = ?config.devices,
            "initialized block I/O tracking"
        );
        Ok(Self {
            skel,
            ringbuf,
            callback,
            registry,
            config,
            last_sample_ns: get_monotonic_timestamp(),
            previous_stats: HashMap::new(),
            previous_depth: HashMap::new(),
            series_ids: HashMap::new(),
        })
    }

    fn read_stats(&self) -> Result<HashMap<u32, DeviceStats>, BpfError> {
        self.skel
            .maps
            .device_stats
            .lookup_batch(MAP_BATCH_SIZE, MapFlags::ANY, MapFlags::ANY)
            .map_err(|error| {
                BpfError::MapError(format!("failed to read block_io device stats: {error}"))
            })?
            .map(|(key, value)| {
                Ok((
                    parse_plain::<u32>(&key, "block_io device key")?,
                    parse_plain::<DeviceStats>(&value, "block_io device stats")?,
                ))
            })
            .collect()
    }

    fn read_depth(&self) -> Result<HashMap<u32, DeviceDepth>, BpfError> {
        self.skel
            .maps
            .device_depths
            .lookup_batch(MAP_BATCH_SIZE, MapFlags::LOCK, MapFlags::ANY)
            .map_err(|error| {
                BpfError::MapError(format!("failed to read block_io device depth: {error}"))
            })?
            .map(|(key, value)| {
                Ok((
                    parse_plain::<u32>(&key, "block_io depth key")?,
                    parse_plain::<DeviceDepth>(&value, "block_io depth stats")?,
                ))
            })
            .collect()
    }

    fn emit_counter(&mut self, dev: u32, sample: CounterSample, timestamp: u64) -> i32 {
        let (device_track, result) = self.registry.borrow_mut().ensure_track(dev, &self.callback);
        if result != 0 {
            return result;
        }
        let key = SeriesKey {
            dev,
            operation: sample.operation,
            kind: sample.kind,
        };
        let (id, new_track) = match self.series_ids.get(&key) {
            Some(id) => (*id, false),
            None => {
                let id = rand::thread_rng().gen::<u64>();
                self.series_ids.insert(key, id);
                (id, true)
            }
        };
        if new_track {
            let result = self.callback.borrow_mut()(Message::Event(Event::Track(Track {
                name: &sample.name,
                track_type: TrackType::Counter {
                    id,
                    unit: Some(sample.unit),
                },
                parent: Some(TrackType::Custom { id: device_track }),
            })));
            if result != 0 {
                return result;
            }
        }
        self.callback.borrow_mut()(Message::Event(Event::Counter(Counter {
            name: &sample.name,
            value: sample.value,
            timestamp,
            track_id: TrackId::Counter { id },
            labels: Cow::Owned(Labels::new()),
            unit: Some(sample.unit),
        })))
    }

    fn operation_samples(
        &self,
        operation: BlockOperation,
        stats: OperationStats,
        elapsed_seconds: f64,
    ) -> Vec<CounterSample> {
        let prefix = operation.name();
        let mut samples = Vec::new();
        if self.config.has_metric(BlockMetric::Throughput) {
            samples.push(counter_sample(
                Some(operation),
                CounterKind::Throughput,
                format!("{prefix}_throughput"),
                stats.bytes as f64 / elapsed_seconds,
                "bytes/s",
            ));
        }
        if self.config.has_metric(BlockMetric::Iops) {
            samples.push(counter_sample(
                Some(operation),
                CounterKind::Iops,
                format!("{prefix}_iops"),
                stats.operations as f64 / elapsed_seconds,
                "operations/s",
            ));
        }
        if self.config.has_metric(BlockMetric::Latency) {
            let operations = stats.operations.max(1) as f64;
            for (kind, suffix, value) in [
                (
                    CounterKind::QueueAverage,
                    "queue_latency_avg",
                    stats.queue_latency_ns as f64 / operations / 1000.0,
                ),
                (
                    CounterKind::ServiceAverage,
                    "service_latency_avg",
                    stats.service_latency_ns as f64 / operations / 1000.0,
                ),
                (
                    CounterKind::TotalAverage,
                    "total_latency_avg",
                    stats.total_latency_ns as f64 / operations / 1000.0,
                ),
                (
                    CounterKind::ServiceP50,
                    "service_latency_p50",
                    histogram_percentile(&stats.service_histogram, 0.50),
                ),
                (
                    CounterKind::ServiceP95,
                    "service_latency_p95",
                    histogram_percentile(&stats.service_histogram, 0.95),
                ),
                (
                    CounterKind::ServiceP99,
                    "service_latency_p99",
                    histogram_percentile(&stats.service_histogram, 0.99),
                ),
                (
                    CounterKind::TotalP50,
                    "total_latency_p50",
                    histogram_percentile(&stats.total_histogram, 0.50),
                ),
                (
                    CounterKind::TotalP95,
                    "total_latency_p95",
                    histogram_percentile(&stats.total_histogram, 0.95),
                ),
                (
                    CounterKind::TotalP99,
                    "total_latency_p99",
                    histogram_percentile(&stats.total_histogram, 0.99),
                ),
            ] {
                samples.push(counter_sample(
                    Some(operation),
                    kind,
                    format!("{prefix}_{suffix}"),
                    value,
                    "us",
                ));
            }
        }
        if self.config.has_metric(BlockMetric::Errors) {
            samples.push(counter_sample(
                Some(operation),
                CounterKind::Errors,
                format!("{prefix}_errors"),
                stats.errors as f64,
                "count",
            ));
        }
        samples
    }

    fn depth_samples(&self, depth: DepthSnapshot, elapsed_ns: u64) -> Vec<CounterSample> {
        let elapsed = elapsed_ns.max(1) as f64;
        vec![
            counter_sample(
                None,
                CounterKind::BusyPercent,
                "busy_percent".to_string(),
                (depth.busy_ns as f64 * 100.0 / elapsed).clamp(0.0, 100.0),
                "percent",
            ),
            counter_sample(
                None,
                CounterKind::SaturatedPercent,
                "saturated_percent".to_string(),
                (depth.saturated_ns as f64 * 100.0 / elapsed).clamp(0.0, 100.0),
                "percent",
            ),
            counter_sample(
                None,
                CounterKind::AverageInflight,
                "average_inflight".to_string(),
                depth.inflight_ns as f64 / elapsed,
                "requests",
            ),
            counter_sample(
                None,
                CounterKind::AverageQueued,
                "average_queued".to_string(),
                depth.queued_ns as f64 / elapsed,
                "requests",
            ),
            counter_sample(
                None,
                CounterKind::Inflight,
                "inflight".to_string(),
                depth.inflight as f64,
                "requests",
            ),
            counter_sample(
                None,
                CounterKind::Queued,
                "queued".to_string(),
                depth.queued as f64,
                "requests",
            ),
            counter_sample(
                None,
                CounterKind::MaxInflight,
                "max_inflight".to_string(),
                depth.max_inflight as f64,
                "requests",
            ),
            counter_sample(
                None,
                CounterKind::MaxQueued,
                "max_queued".to_string(),
                depth.max_queued as f64,
                "requests",
            ),
        ]
    }

    fn sample(&mut self, now: u64) -> Result<(), BpfError> {
        let elapsed_ns = now.saturating_sub(self.last_sample_ns).max(1);
        let elapsed_seconds = elapsed_ns as f64 / 1_000_000_000.0;
        let stats = self.read_stats()?;
        let depths = if self.config.has_metric(BlockMetric::Saturation) {
            self.read_depth()?
        } else {
            HashMap::new()
        };
        let mut devices = stats.keys().copied().collect::<HashSet<_>>();
        devices.extend(depths.iter().filter_map(|(dev, depth)| {
            (depth.busy_ns > 0
                || depth.saturated_ns > 0
                || depth.inflight > 0
                || depth.queued > 0
                || depth.max_inflight > 0
                || depth.max_queued > 0)
                .then_some(*dev)
        }));

        for dev in devices {
            if let Some(current) = stats.get(&dev).copied() {
                let previous = self.previous_stats.insert(dev, current);
                let delta = current.delta(previous);
                let operations = self.config.operations.clone();
                for operation in operations {
                    for sample in self.operation_samples(
                        operation,
                        delta.operations[operation.index()],
                        elapsed_seconds,
                    ) {
                        if self.emit_counter(dev, sample, now) != 0 {
                            break;
                        }
                    }
                }
                if self.config.has_metric(BlockMetric::Errors) {
                    for sample in [
                        counter_sample(
                            None,
                            CounterKind::Requeues,
                            "requeues".to_string(),
                            delta.requeues as f64,
                            "count",
                        ),
                        counter_sample(
                            None,
                            CounterKind::LostRequests,
                            "lost_requests".to_string(),
                            delta.lost_requests as f64,
                            "count",
                        ),
                    ] {
                        if self.emit_counter(dev, sample, now) != 0 {
                            break;
                        }
                    }
                }
                if self.config.timeline
                    && self.emit_counter(
                        dev,
                        counter_sample(
                            None,
                            CounterKind::TimelineDrops,
                            "timeline_drops".to_string(),
                            delta.timeline_drops as f64,
                            "count",
                        ),
                        now,
                    ) != 0
                {
                    continue;
                }
            }
            if let Some(current) = depths.get(&dev).copied() {
                let snapshot = current.snapshot(now);
                let previous = self.previous_depth.insert(dev, snapshot);
                let delta = snapshot.delta(previous);
                for sample in self.depth_samples(delta, elapsed_ns) {
                    if self.emit_counter(dev, sample, now) != 0 {
                        break;
                    }
                }
            }
        }
        self.last_sample_ns = now;
        Ok(())
    }

    fn sample_if_due(&mut self, force: bool) -> Result<(), BpfError> {
        let now = get_monotonic_timestamp();
        let interval = 1_000_000_000u64 / self.config.frequency;
        if force || now.saturating_sub(self.last_sample_ns) >= interval {
            self.sample(now)?;
        }
        Ok(())
    }

    pub fn consume(&mut self) -> Result<(), BpfError> {
        if let Some(ringbuf) = &self.ringbuf {
            ringbuf.consume().map_err(|error| {
                BpfError::MapError(format!("failed to consume block_io spans: {error}"))
            })?;
        }
        self.sample_if_due(false)
    }

    pub fn poll(&mut self, timeout: Duration) -> Result<(), BpfError> {
        if let Some(ringbuf) = &self.ringbuf {
            ringbuf.poll(timeout).map_err(|error| {
                BpfError::MapError(format!("failed to poll block_io spans: {error}"))
            })?;
        }
        self.sample_if_due(false)
    }

    pub fn flush(&mut self) -> Result<(), BpfError> {
        if let Some(ringbuf) = &self.ringbuf {
            ringbuf.consume().map_err(|error| {
                BpfError::MapError(format!("failed to flush block_io spans: {error}"))
            })?;
        }
        self.sample_if_due(true)
    }
}

fn counter_sample(
    operation: Option<BlockOperation>,
    kind: CounterKind,
    name: String,
    value: f64,
    unit: &'static str,
) -> CounterSample {
    CounterSample {
        operation,
        kind,
        name,
        value,
        unit,
    }
}

fn parse_plain<T: plain::Plain + Copy>(data: &[u8], description: &str) -> Result<T, BpfError> {
    plain::from_bytes::<T>(data)
        .copied()
        .map_err(|error| BpfError::MapError(format!("failed to parse {description}: {error:?}")))
}

fn histogram_percentile(histogram: &[u64; HISTOGRAM_BUCKETS], quantile: f64) -> f64 {
    let total: u64 = histogram.iter().sum();
    if total == 0 {
        return 0.0;
    }
    let target = (total as f64 * quantile).ceil() as u64;
    let mut cumulative = 0;
    for (bucket, count) in histogram.iter().enumerate() {
        cumulative += count;
        if cumulative >= target {
            return if bucket == 0 {
                1.0
            } else {
                (1u64 << (bucket + 1).min(32)) as f64
            };
        }
    }
    (1u64 << 32) as f64
}

fn emit_request_span<F>(
    callback: &Rc<RefCell<F>>,
    registry: &Rc<RefCell<DeviceRegistry>>,
    event: RequestSpanEvent,
) -> i32
where
    F: for<'a> FnMut(Message<'a>) -> i32,
{
    let operation = BlockOperation::from_index(event.operation).unwrap_or(BlockOperation::Other);
    let (track_id, result) = registry.borrow_mut().ensure_track(event.dev, callback);
    if result != 0 {
        return result;
    }
    let partition = registry.borrow_mut().info(event.partition_dev);
    let mut labels = Labels::new();
    labels.ints.insert("bytes", event.bytes as i64);
    labels.ints.insert("sector", event.sector as i64);
    labels.ints.insert("pid", event.pid as i64);
    labels.ints.insert("tgid", event.tgid as i64);
    labels.ints.insert("error", event.error as i64);
    labels.ints.insert("requeues", event.requeues as i64);
    labels
        .floats
        .insert("queue_latency_us", event.queue_ns as f64 / 1000.0);
    labels
        .floats
        .insert("service_latency_us", event.service_ns as f64 / 1000.0);
    labels
        .floats
        .insert("total_latency_us", event.total_ns as f64 / 1000.0);
    labels.bools.insert("completed", event.completed != 0);
    labels.strings.insert("comm", Cow::Owned(event.comm()));
    labels
        .strings
        .insert("operation", Cow::Borrowed(operation.name()));
    labels.strings.insert(
        "partition",
        Cow::Owned(format!("{} ({})", partition.name, partition.major_minor())),
    );
    if !partition.mounts.is_empty() {
        let mut filesystems = partition
            .mounts
            .iter()
            .map(|mount| mount.filesystem.as_str())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        filesystems.sort_unstable();
        labels
            .strings
            .insert("filesystems", Cow::Owned(filesystems.join(",")));
        labels.strings.insert(
            "mountpoints",
            Cow::Owned(
                partition
                    .mounts
                    .iter()
                    .map(|mount| mount.mountpoint.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
        labels.strings.insert(
            "sources",
            Cow::Owned(
                partition
                    .mounts
                    .iter()
                    .map(|mount| mount.source.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
    }
    let status = if event.completed != 0 {
        if event.error == 0 {
            operation.name()
        } else {
            "error"
        }
    } else {
        "requeued"
    };
    let name = format!("{status} {}", format_bytes(event.bytes));
    callback.borrow_mut()(Message::Event(Event::Span(Span {
        name: &name,
        span_id: event.request_id ^ event.start_ns,
        start_timestamp: event.start_ns,
        end_timestamp: event.end_ns,
        track_id: TrackId::Custom { id: track_id },
        labels: Cow::Owned(labels),
    })))
}

fn format_bytes(bytes: u32) -> String {
    if bytes > 0 && bytes.is_multiple_of(1024 * 1024) {
        format!("{} MiB", bytes / (1024 * 1024))
    } else if bytes > 0 && bytes.is_multiple_of(1024) {
        format!("{} KiB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn encode_device(major: u64, minor: u64) -> Result<u32, BpfError> {
    if major >= (1 << 12) || minor >= (1 << 20) {
        return Err(BpfError::LoadError(format!(
            "block device number is out of range: {major}:{minor}"
        )));
    }
    Ok(((major as u32) << 20) | minor as u32)
}

fn decode_device(dev: u32) -> (u32, u32) {
    (dev >> 20, dev & ((1 << 20) - 1))
}

fn format_device(dev: u32) -> String {
    let (major, minor) = decode_device(dev);
    format!("{major}:{minor}")
}

fn parse_major_minor(value: &str) -> Result<u32, BpfError> {
    let (major, minor) = value
        .split_once(':')
        .ok_or_else(|| BpfError::LoadError(format!("invalid block device number: {value}")))?;
    let major = major
        .parse::<u64>()
        .map_err(|error| BpfError::LoadError(format!("invalid block device {value}: {error}")))?;
    let minor = minor
        .parse::<u64>()
        .map_err(|error| BpfError::LoadError(format!("invalid block device {value}: {error}")))?;
    encode_device(major, minor)
}

fn sysfs_device_number(path: &Path) -> Result<u32, BpfError> {
    let value = fs::read_to_string(path.join("dev")).map_err(|error| {
        BpfError::LoadError(format!(
            "failed to read block device {}: {error}",
            path.display()
        ))
    })?;
    parse_major_minor(value.trim())
}

fn physical_device(dev: u32) -> Result<u32, BpfError> {
    let (major, minor) = decode_device(dev);
    let path = fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).map_err(|error| {
        BpfError::LoadError(format!(
            "failed to resolve block device {major}:{minor}: {error}"
        ))
    })?;
    if path.join("partition").exists() {
        let parent = path.parent().ok_or_else(|| {
            BpfError::LoadError(format!("block partition has no parent: {}", path.display()))
        })?;
        sysfs_device_number(parent)
    } else {
        Ok(dev)
    }
}

fn resolve_device_filter(value: &str) -> Result<u32, BpfError> {
    let dev = if value.contains(':') && !value.contains('/') {
        parse_major_minor(value)?
    } else {
        let path = Path::new(value);
        if path.is_absolute() {
            let metadata = fs::metadata(path).map_err(|error| {
                BpfError::LoadError(format!("failed to inspect device {value}: {error}"))
            })?;
            encode_device(major(metadata.rdev()), minor(metadata.rdev()))?
        } else {
            sysfs_device_number(Path::new("/sys/class/block").join(value).as_path())?
        }
    };
    physical_device(dev)
}

fn discover_physical_devices() -> Result<HashSet<u32>, BpfError> {
    fs::read_dir("/sys/class/block")
        .map_err(|error| {
            BpfError::LoadError(format!("failed to enumerate block devices: {error}"))
        })?
        .map(|entry| {
            let entry = entry.map_err(|error| {
                BpfError::LoadError(format!("failed to enumerate block devices: {error}"))
            })?;
            physical_device(sysfs_device_number(&entry.path())?)
        })
        .collect()
}

fn decode_mount_field(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn read_mountinfo() -> std::io::Result<HashMap<u32, Vec<MountInfo>>> {
    let content = fs::read_to_string("/proc/self/mountinfo")?;
    let mut mounts = HashMap::<u32, Vec<MountInfo>>::new();
    for line in content.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        if fields.len() <= separator + 2 || fields.len() < 5 {
            continue;
        }
        let Ok(dev) = parse_major_minor(fields[2]) else {
            continue;
        };
        mounts.entry(dev).or_default().push(MountInfo {
            filesystem: fields[separator + 1].to_string(),
            source: decode_mount_field(fields[separator + 2]),
            mountpoint: decode_mount_field(fields[4]),
        });
    }
    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_keeps_request_timeline_disabled() {
        let config = BlockIoConfig::default();
        assert_eq!(config.frequency, 9);
        assert_eq!(
            config.operations,
            vec![BlockOperation::Read, BlockOperation::Write]
        );
        assert!(!config.timeline);
        assert_eq!(config.max_requests, 32768);
    }

    #[test]
    fn device_number_round_trip() {
        let encoded = encode_device(259, 17).unwrap();
        assert_eq!(decode_device(encoded), (259, 17));
        assert_eq!(parse_major_minor("259:17").unwrap(), encoded);
    }

    #[test]
    fn stats_delta_handles_counters_and_histograms() {
        let mut previous = DeviceStats {
            first_seen_ns: 10,
            ..DeviceStats::default()
        };
        previous.operations[0].bytes = 100;
        previous.operations[0].service_histogram[4] = 2;
        let mut current = previous;
        current.operations[0].bytes = 350;
        current.operations[0].service_histogram[4] = 5;
        let delta = current.delta(Some(previous));
        assert_eq!(delta.operations[0].bytes, 250);
        assert_eq!(delta.operations[0].service_histogram[4], 3);
    }

    #[test]
    fn recreated_stats_are_not_subtracted() {
        let mut previous = DeviceStats {
            first_seen_ns: 10,
            ..DeviceStats::default()
        };
        previous.operations[1].bytes = 500;
        let mut current = DeviceStats {
            first_seen_ns: 20,
            ..DeviceStats::default()
        };
        current.operations[1].bytes = 25;
        assert_eq!(current.delta(Some(previous)).operations[1].bytes, 25);
    }

    #[test]
    fn histogram_percentiles_use_power_of_two_upper_bounds() {
        let mut histogram = [0; HISTOGRAM_BUCKETS];
        histogram[3] = 90;
        histogram[7] = 10;
        assert_eq!(histogram_percentile(&histogram, 0.50), 16.0);
        assert_eq!(histogram_percentile(&histogram, 0.99), 256.0);
    }

    #[test]
    fn live_depth_is_extended_to_sample_time() {
        let depth = DeviceDepth {
            first_seen_ns: 1,
            last_update_ns: 100,
            inflight: 2,
            queued: 1,
            ..DeviceDepth::default()
        };
        let snapshot = depth.snapshot(200);
        assert_eq!(snapshot.busy_ns, 100);
        assert_eq!(snapshot.saturated_ns, 100);
        assert_eq!(snapshot.inflight_ns, 200);
        assert_eq!(snapshot.queued_ns, 100);
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let config = BlockIoConfig {
            frequency: 0,
            ..BlockIoConfig::default()
        };
        assert!(config.validate().is_err());
        let config = BlockIoConfig {
            timeline: true,
            ringbuf: 5000,
            ..BlockIoConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn mount_fields_are_unescaped() {
        assert_eq!(
            decode_mount_field("/path\\040with\\040spaces"),
            "/path with spaces"
        );
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use std::io::Write;
    use std::thread;

    #[test]
    #[ignore = "requires root and block tracepoint access"]
    fn captures_block_writes_and_request_spans() {
        assert_eq!(unsafe { libc::geteuid() }, 0);
        let spans = Rc::new(RefCell::new(0usize));
        let spans_ref = spans.clone();
        let mut object = Object::new(BlockIoConfig {
            frequency: 20,
            timeline: true,
            ringbuf: 1024 * 1024,
            ..BlockIoConfig::default()
        });
        let mut tracker = object
            .build(move |message| {
                if matches!(message, Message::Event(Event::Span(_))) {
                    *spans_ref.borrow_mut() += 1;
                }
                0
            })
            .expect("failed to build block I/O tracker");

        let mut file = tempfile::tempfile().expect("failed to create temporary file");
        let buffer = vec![0x5a; 1024 * 1024];
        for _ in 0..16 {
            file.write_all(&buffer).expect("failed to write test data");
        }
        file.sync_all().expect("failed to sync test data");

        for _ in 0..20 {
            tracker.consume().expect("failed to consume block events");
            thread::sleep(Duration::from_millis(25));
        }
        tracker.flush().expect("failed to flush block counters");

        let stats = tracker.read_stats().expect("failed to read block stats");
        let depths = tracker.read_depth().expect("failed to read block depths");
        let write_bytes: u64 = stats
            .values()
            .map(|stats| stats.operations[BlockOperation::Write.index()].bytes)
            .sum();
        assert!(write_bytes > 0, "expected completed block write bytes");
        assert!(!depths.is_empty(), "expected block device depth state");
        assert!(*spans.borrow() > 0, "expected block request spans");
    }
}
