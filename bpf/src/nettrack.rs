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

mod nettrack_skel {
    include!(concat!(env!("OUT_DIR"), "/nettrack.skel.rs"));
}

use nettrack_skel::*;

use crate::{get_monotonic_timestamp, BpfError, Filterable};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{MapCore, MapFlags, OpenObject};
use protocol::{Counter, Event, Labels, Message, Track, TrackId, TrackType};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hash;
use std::mem::MaybeUninit;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use tracing::debug;

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const TCP_INDEX: usize = 0;
const UDP_INDEX: usize = 1;
const MAP_BATCH_SIZE: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetProtocol {
    Tcp,
    Udp,
}

impl NetProtocol {
    fn index(self) -> usize {
        match self {
            Self::Tcp => TCP_INDEX,
            Self::Udp => UDP_INDEX,
        }
    }

    fn from_index(index: u8) -> Option<Self> {
        match index as usize {
            TCP_INDEX => Some(Self::Tcp),
            UDP_INDEX => Some(Self::Udp),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetDirection {
    Send,
    Receive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetScope {
    Host,
    Process,
    Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetMetric {
    Bytes,
    Operations,
    Errors,
}

fn default_frequency() -> u64 {
    9
}

fn default_peer_frequency() -> u64 {
    1
}

fn default_ringbuf_size() -> usize {
    1024 * 1024
}

fn default_protocols() -> Vec<NetProtocol> {
    vec![NetProtocol::Tcp, NetProtocol::Udp]
}

fn default_directions() -> Vec<NetDirection> {
    vec![NetDirection::Send, NetDirection::Receive]
}

fn default_scopes() -> Vec<NetScope> {
    vec![NetScope::Host]
}

fn default_metrics() -> Vec<NetMetric> {
    vec![NetMetric::Bytes]
}

fn default_process_entries() -> u32 {
    1024
}

fn default_peer_entries() -> u32 {
    4096
}

fn default_peer_tracks() -> usize {
    512
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetTrackConfig {
    #[serde(default = "default_frequency")]
    pub frequency: u64,
    #[serde(default = "default_peer_frequency")]
    pub peer_frequency: u64,
    /// Kept for configuration compatibility. Map-based sampling has no ring buffer.
    #[serde(default = "default_ringbuf_size")]
    pub ringbuf: usize,
    #[serde(default)]
    pub scaled: bool,
    #[serde(default = "default_protocols")]
    pub protocols: Vec<NetProtocol>,
    #[serde(default = "default_directions")]
    pub directions: Vec<NetDirection>,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<NetScope>,
    #[serde(default = "default_metrics")]
    pub metrics: Vec<NetMetric>,
    #[serde(default)]
    pub pid_filters: Vec<i32>,
    #[serde(default)]
    pub filter_process: Vec<String>,
    #[serde(default = "default_process_entries")]
    pub max_process_entries: u32,
    #[serde(default = "default_peer_entries")]
    pub max_peer_entries: u32,
    #[serde(default = "default_peer_tracks")]
    pub max_peer_tracks: usize,
}

impl Default for NetTrackConfig {
    fn default() -> Self {
        Self {
            frequency: default_frequency(),
            peer_frequency: default_peer_frequency(),
            ringbuf: default_ringbuf_size(),
            scaled: false,
            protocols: default_protocols(),
            directions: default_directions(),
            scopes: default_scopes(),
            metrics: default_metrics(),
            pid_filters: Vec::new(),
            filter_process: Vec::new(),
            max_process_entries: default_process_entries(),
            max_peer_entries: default_peer_entries(),
            max_peer_tracks: default_peer_tracks(),
        }
    }
}

impl NetTrackConfig {
    fn has_protocol(&self, protocol: NetProtocol) -> bool {
        self.protocols.contains(&protocol)
    }

    fn has_direction(&self, direction: NetDirection) -> bool {
        self.directions.contains(&direction)
    }

    fn has_scope(&self, scope: NetScope) -> bool {
        self.scopes.contains(&scope)
    }

    fn has_metric(&self, metric: NetMetric) -> bool {
        self.metrics.contains(&metric)
    }

    fn validate(&self) -> Result<(), BpfError> {
        if self.frequency == 0 {
            return Err(BpfError::LoadError(
                "nettrack frequency must be greater than zero".to_string(),
            ));
        }
        if self.has_scope(NetScope::Peer) && self.peer_frequency == 0 {
            return Err(BpfError::LoadError(
                "nettrack peer_frequency must be greater than zero".to_string(),
            ));
        }
        if self.protocols.is_empty()
            || self.directions.is_empty()
            || self.scopes.is_empty()
            || self.metrics.is_empty()
        {
            return Err(BpfError::LoadError(
                "nettrack protocols, directions, scopes, and metrics must not be empty".to_string(),
            ));
        }
        if self.has_scope(NetScope::Process) && self.max_process_entries == 0 {
            return Err(BpfError::LoadError(
                "nettrack max_process_entries must be greater than zero".to_string(),
            ));
        }
        if self.has_scope(NetScope::Peer)
            && (self.max_peer_entries == 0 || self.max_peer_tracks == 0)
        {
            return Err(BpfError::LoadError(
                "nettrack peer entry and track limits must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

pub struct Object {
    object: MaybeUninit<libbpf_rs::OpenObject>,
    config: NetTrackConfig,
}

impl Object {
    pub fn new(config: NetTrackConfig) -> Self {
        Self {
            object: MaybeUninit::uninit(),
            config,
        }
    }

    pub fn build<'bd, F>(&'bd mut self, callback: F) -> Result<NetTrack<'bd, F>, BpfError>
    where
        F: for<'a> FnMut(Message<'a>) -> i32 + 'bd,
    {
        self.config.validate()?;
        NetTrack::new(&mut self.object, self.config.clone(), callback)
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StatsValue {
    send_bytes: u64,
    recv_bytes: u64,
    send_operations: u64,
    recv_operations: u64,
    send_errors: u64,
    recv_errors: u64,
    send_would_block: u64,
    recv_would_block: u64,
    tcp_recv_eof: u64,
    udp_recv_truncated: u64,
    first_seen_ns: u64,
    last_seen_ns: u64,
}

unsafe impl plain::Plain for StatsValue {}

impl StatsValue {
    fn accumulate(&mut self, other: Self) {
        self.send_bytes = self.send_bytes.saturating_add(other.send_bytes);
        self.recv_bytes = self.recv_bytes.saturating_add(other.recv_bytes);
        self.send_operations = self.send_operations.saturating_add(other.send_operations);
        self.recv_operations = self.recv_operations.saturating_add(other.recv_operations);
        self.send_errors = self.send_errors.saturating_add(other.send_errors);
        self.recv_errors = self.recv_errors.saturating_add(other.recv_errors);
        self.send_would_block = self.send_would_block.saturating_add(other.send_would_block);
        self.recv_would_block = self.recv_would_block.saturating_add(other.recv_would_block);
        self.tcp_recv_eof = self.tcp_recv_eof.saturating_add(other.tcp_recv_eof);
        self.udp_recv_truncated = self
            .udp_recv_truncated
            .saturating_add(other.udp_recv_truncated);
        if self.first_seen_ns == 0
            || (other.first_seen_ns != 0 && other.first_seen_ns < self.first_seen_ns)
        {
            self.first_seen_ns = other.first_seen_ns;
        }
        self.last_seen_ns = self.last_seen_ns.max(other.last_seen_ns);
    }

    fn delta(self, previous: Option<Self>) -> Self {
        let Some(previous) = previous.filter(|p| p.first_seen_ns == self.first_seen_ns) else {
            return self;
        };
        Self {
            send_bytes: self.send_bytes.saturating_sub(previous.send_bytes),
            recv_bytes: self.recv_bytes.saturating_sub(previous.recv_bytes),
            send_operations: self
                .send_operations
                .saturating_sub(previous.send_operations),
            recv_operations: self
                .recv_operations
                .saturating_sub(previous.recv_operations),
            send_errors: self.send_errors.saturating_sub(previous.send_errors),
            recv_errors: self.recv_errors.saturating_sub(previous.recv_errors),
            send_would_block: self
                .send_would_block
                .saturating_sub(previous.send_would_block),
            recv_would_block: self
                .recv_would_block
                .saturating_sub(previous.recv_would_block),
            tcp_recv_eof: self.tcp_recv_eof.saturating_sub(previous.tcp_recv_eof),
            udp_recv_truncated: self
                .udp_recv_truncated
                .saturating_sub(previous.udp_recv_truncated),
            first_seen_ns: self.first_seen_ns,
            last_seen_ns: self.last_seen_ns,
        }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
struct ProcessKey {
    tgid: u32,
    protocol: u8,
    padding: [u8; 3],
}

unsafe impl plain::Plain for ProcessKey {}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
struct PeerKey {
    tgid: u32,
    protocol: u8,
    family: u8,
    local_port: u16,
    remote_port: u16,
    padding: u16,
    remote_addr: [u8; 16],
}

unsafe impl plain::Plain for PeerKey {}

impl PeerKey {
    fn protocol(self) -> Option<NetProtocol> {
        NetProtocol::from_index(self.protocol)
    }

    fn remote_ip(self) -> Option<IpAddr> {
        match self.family {
            AF_INET => Some(IpAddr::V4(Ipv4Addr::new(
                self.remote_addr[0],
                self.remote_addr[1],
                self.remote_addr[2],
                self.remote_addr[3],
            ))),
            AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(self.remote_addr))),
            _ => None,
        }
    }

    fn track_name(self) -> String {
        let protocol = self.protocol().map(NetProtocol::name).unwrap_or("network");
        match self.remote_ip() {
            Some(IpAddr::V4(ip)) => format!("{protocol} {ip}:{}", self.remote_port),
            Some(IpAddr::V6(ip)) => format!("{protocol} [{ip}]:{}", self.remote_port),
            None => format!("{protocol} unknown-peer"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ScopeKey {
    Host,
    Process(u32),
    Peer(PeerKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CounterKind {
    SendBytes,
    RecvBytes,
    SendOperations,
    RecvOperations,
    SendErrors,
    RecvErrors,
    SendWouldBlock,
    RecvWouldBlock,
    TcpRecvEof,
    UdpRecvTruncated,
    TotalSendBytes,
    TotalRecvBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SeriesKey {
    scope: ScopeKey,
    protocol: NetProtocol,
    kind: CounterKind,
}

struct CounterSample {
    kind: CounterKind,
    name: String,
    value: f64,
    unit: &'static str,
}

pub struct NetTrack<'this, F> {
    skel: NettrackSkel<'this>,
    callback: F,
    config: NetTrackConfig,
    last_sample_ns: u64,
    last_peer_sample_ns: u64,
    global_previous: [StatsValue; 2],
    process_previous: HashMap<ProcessKey, StatsValue>,
    peer_previous: HashMap<PeerKey, StatsValue>,
    series_ids: HashMap<SeriesKey, u64>,
    submitted_tracks: HashSet<TrackId>,
    peer_track_ids: HashMap<PeerKey, u64>,
}

impl<'this, F> NetTrack<'this, F>
where
    F: for<'a> FnMut(Message<'a>) -> i32 + 'this,
{
    fn new(
        open_object: &'this mut MaybeUninit<OpenObject>,
        config: NetTrackConfig,
        callback: F,
    ) -> Result<Self, BpfError> {
        let skel_builder = NettrackSkelBuilder::default();
        let mut open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {e}")))?;

        let process_entries = if config.has_scope(NetScope::Process) {
            config.max_process_entries
        } else {
            1
        };
        let peer_entries = if config.has_scope(NetScope::Peer) {
            config.max_peer_entries
        } else {
            1
        };
        open_skel
            .maps
            .process_stats
            .set_max_entries(process_entries)
            .map_err(|e| BpfError::LoadError(format!("failed to size process map: {e}")))?;
        open_skel
            .maps
            .peer_stats
            .set_max_entries(peer_entries)
            .map_err(|e| BpfError::LoadError(format!("failed to size peer map: {e}")))?;

        let filter_enabled = !config.pid_filters.is_empty() || !config.filter_process.is_empty();
        let rodata = open_skel.maps.rodata_data.as_mut().unwrap();
        rodata
            .cfg
            .tcp_enabled
            .write(config.has_protocol(NetProtocol::Tcp));
        rodata
            .cfg
            .udp_enabled
            .write(config.has_protocol(NetProtocol::Udp));
        rodata
            .cfg
            .send_enabled
            .write(config.has_direction(NetDirection::Send));
        rodata
            .cfg
            .receive_enabled
            .write(config.has_direction(NetDirection::Receive));
        rodata
            .cfg
            .host_enabled
            .write(config.has_scope(NetScope::Host));
        rodata
            .cfg
            .process_enabled
            .write(config.has_scope(NetScope::Process));
        rodata
            .cfg
            .peer_enabled
            .write(config.has_scope(NetScope::Peer));
        rodata
            .cfg
            .bytes_enabled
            .write(config.has_metric(NetMetric::Bytes));
        rodata
            .cfg
            .operations_enabled
            .write(config.has_metric(NetMetric::Operations));
        rodata
            .cfg
            .errors_enabled
            .write(config.has_metric(NetMetric::Errors));
        rodata.cfg.filter_enabled.write(filter_enabled);

        let tcp_enabled = config.has_protocol(NetProtocol::Tcp);
        let udp_enabled = config.has_protocol(NetProtocol::Udp);
        let send_enabled = config.has_direction(NetDirection::Send);
        let receive_enabled = config.has_direction(NetDirection::Receive);
        open_skel
            .progs
            .trace_tcp_sendmsg
            .set_autoload(tcp_enabled && send_enabled);
        open_skel
            .progs
            .trace_tcp_recvmsg
            .set_autoload(tcp_enabled && receive_enabled);
        open_skel
            .progs
            .trace_udp_sendmsg
            .set_autoload(udp_enabled && send_enabled);
        open_skel
            .progs
            .trace_udp_recvmsg
            .set_autoload(udp_enabled && receive_enabled);

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {e}")))?;

        for &pid in &config.pid_filters {
            let pid = u32::try_from(pid).map_err(|_| {
                BpfError::LoadError(format!("nettrack PID filter must be positive: {pid}"))
            })?;
            skel.maps
                .tracked_tgids
                .update(&pid.to_ne_bytes(), &1u32.to_ne_bytes(), MapFlags::ANY)
                .map_err(|e| BpfError::MapError(format!("failed to add PID filter: {e}")))?;
        }

        skel.attach()
            .map_err(|e| BpfError::AttachError(format!("failed to attach bpf programs: {e}")))?;

        debug!(
            protocols = ?config.protocols,
            directions = ?config.directions,
            scopes = ?config.scopes,
            metrics = ?config.metrics,
            frequency = config.frequency,
            peer_frequency = config.peer_frequency,
            "initialized map-based network tracking"
        );

        let now = get_monotonic_timestamp();
        Ok(Self {
            skel,
            callback,
            config,
            last_sample_ns: now,
            last_peer_sample_ns: now,
            global_previous: [StatsValue::default(); 2],
            process_previous: HashMap::new(),
            peer_previous: HashMap::new(),
            series_ids: HashMap::new(),
            submitted_tracks: HashSet::new(),
            peer_track_ids: HashMap::new(),
        })
    }

    fn parse_value(data: &[u8], kind: &str) -> Result<StatsValue, BpfError> {
        plain::from_bytes::<StatsValue>(data)
            .copied()
            .map_err(|e| BpfError::MapError(format!("failed to parse {kind}: {e:?}")))
    }

    fn parse_key<K: plain::Plain + Copy>(data: &[u8], kind: &str) -> Result<K, BpfError> {
        plain::from_bytes::<K>(data)
            .copied()
            .map_err(|e| BpfError::MapError(format!("failed to parse {kind}: {e:?}")))
    }

    fn read_global(&self, protocol: NetProtocol) -> Result<StatsValue, BpfError> {
        let key = (protocol.index() as u32).to_ne_bytes();
        let values = self
            .skel
            .maps
            .global_stats
            .lookup_percpu(&key, MapFlags::ANY)
            .map_err(|e| BpfError::MapError(format!("failed to read host network map: {e}")))?
            .unwrap_or_default();
        let mut total = StatsValue::default();
        for value in values {
            total.accumulate(Self::parse_value(&value, "host network statistics")?);
        }
        Ok(total)
    }

    fn read_processes(&self) -> Result<Vec<(ProcessKey, StatsValue)>, BpfError> {
        self.skel
            .maps
            .process_stats
            .lookup_batch(MAP_BATCH_SIZE, MapFlags::ANY, MapFlags::ANY)
            .map_err(|e| BpfError::MapError(format!("failed to read process network map: {e}")))?
            .map(|(key, value)| {
                Ok((
                    Self::parse_key(&key, "process network key")?,
                    Self::parse_value(&value, "process network statistics")?,
                ))
            })
            .collect()
    }

    fn read_peers(&self) -> Result<Vec<(PeerKey, StatsValue)>, BpfError> {
        self.skel
            .maps
            .peer_stats
            .lookup_batch(MAP_BATCH_SIZE, MapFlags::ANY, MapFlags::ANY)
            .map_err(|e| BpfError::MapError(format!("failed to read peer network map: {e}")))?
            .map(|(key, value)| {
                Ok((
                    Self::parse_key(&key, "peer network key")?,
                    Self::parse_value(&value, "peer network statistics")?,
                ))
            })
            .collect()
    }

    fn samples_for(
        &self,
        protocol: NetProtocol,
        stats: StatsValue,
        elapsed_seconds: f64,
    ) -> Vec<CounterSample> {
        let prefix = protocol.name();
        let mut samples = Vec::new();
        if self.config.has_metric(NetMetric::Bytes) {
            let (send_name, recv_name, send_value, recv_value, unit) = if self.config.scaled {
                (
                    format!("{prefix}_send_rate"),
                    format!("{prefix}_recv_rate"),
                    stats.send_bytes as f64 * 8.0 / elapsed_seconds,
                    stats.recv_bytes as f64 * 8.0 / elapsed_seconds,
                    "bits/s",
                )
            } else {
                (
                    format!("{prefix}_send"),
                    format!("{prefix}_recv"),
                    stats.send_bytes as f64,
                    stats.recv_bytes as f64,
                    "bytes",
                )
            };
            samples.push(CounterSample {
                kind: CounterKind::SendBytes,
                name: send_name,
                value: send_value,
                unit,
            });
            samples.push(CounterSample {
                kind: CounterKind::RecvBytes,
                name: recv_name,
                value: recv_value,
                unit,
            });
        }
        if self.config.has_metric(NetMetric::Operations) {
            let operation = if protocol == NetProtocol::Udp {
                "datagrams"
            } else {
                "calls"
            };
            samples.push(CounterSample {
                kind: CounterKind::SendOperations,
                name: format!("{prefix}_send_{operation}"),
                value: stats.send_operations as f64,
                unit: "count",
            });
            samples.push(CounterSample {
                kind: CounterKind::RecvOperations,
                name: format!("{prefix}_recv_{operation}"),
                value: stats.recv_operations as f64,
                unit: "count",
            });
            if protocol == NetProtocol::Tcp {
                samples.push(CounterSample {
                    kind: CounterKind::TcpRecvEof,
                    name: "tcp_recv_eof".to_string(),
                    value: stats.tcp_recv_eof as f64,
                    unit: "count",
                });
            } else {
                samples.push(CounterSample {
                    kind: CounterKind::UdpRecvTruncated,
                    name: "udp_recv_truncated".to_string(),
                    value: stats.udp_recv_truncated as f64,
                    unit: "count",
                });
            }
        }
        if self.config.has_metric(NetMetric::Errors) {
            for (kind, suffix, value) in [
                (CounterKind::SendErrors, "send_errors", stats.send_errors),
                (CounterKind::RecvErrors, "recv_errors", stats.recv_errors),
                (
                    CounterKind::SendWouldBlock,
                    "send_would_block",
                    stats.send_would_block,
                ),
                (
                    CounterKind::RecvWouldBlock,
                    "recv_would_block",
                    stats.recv_would_block,
                ),
            ] {
                samples.push(CounterSample {
                    kind,
                    name: format!("{prefix}_{suffix}"),
                    value: value as f64,
                    unit: "count",
                });
            }
        }
        samples
    }

    fn ensure_peer_track(&mut self, key: PeerKey) -> Option<u64> {
        if let Some(id) = self.peer_track_ids.get(&key) {
            return Some(*id);
        }
        if self.peer_track_ids.len() >= self.config.max_peer_tracks {
            return None;
        }
        let id = rand::thread_rng().gen::<u64>();
        let name = key.track_name();
        let result = (self.callback)(Message::Event(Event::Track(Track {
            name: &name,
            track_type: TrackType::Custom { id },
            parent: Some(TrackType::Process {
                pid: key.tgid as i32,
            }),
        })));
        if result != 0 {
            return None;
        }
        self.peer_track_ids.insert(key, id);
        Some(id)
    }

    fn emit_sample(
        &mut self,
        scope: ScopeKey,
        protocol: NetProtocol,
        sample: CounterSample,
        timestamp: u64,
    ) -> i32 {
        let parent = match scope {
            ScopeKey::Host => None,
            ScopeKey::Process(tgid) => Some(TrackType::Process { pid: tgid as i32 }),
            ScopeKey::Peer(key) => {
                let Some(id) = self.ensure_peer_track(key) else {
                    return 0;
                };
                Some(TrackType::Custom { id })
            }
        };
        let series_key = SeriesKey {
            scope,
            protocol,
            kind: sample.kind,
        };
        let id = *self
            .series_ids
            .entry(series_key)
            .or_insert_with(|| rand::thread_rng().gen::<u64>());
        let track_id = TrackId::Counter { id };
        if self.submitted_tracks.insert(track_id) {
            let result = (self.callback)(Message::Event(Event::Track(Track {
                name: &sample.name,
                track_type: TrackType::Counter {
                    id,
                    unit: Some(sample.unit),
                },
                parent,
            })));
            if result != 0 {
                return result;
            }
        }

        debug!(
            name = sample.name,
            value = sample.value,
            timestamp,
            "emitting network counter"
        );
        (self.callback)(Message::Event(Event::Counter(Counter {
            name: &sample.name,
            value: sample.value,
            timestamp,
            track_id,
            labels: Cow::Owned(Labels::new()),
            unit: Some(sample.unit),
        })))
    }

    fn emit_stats(
        &mut self,
        scope: ScopeKey,
        protocol: NetProtocol,
        stats: StatsValue,
        elapsed_seconds: f64,
        timestamp: u64,
    ) -> i32 {
        for sample in self.samples_for(protocol, stats, elapsed_seconds) {
            let result = self.emit_sample(scope, protocol, sample, timestamp);
            if result != 0 {
                return result;
            }
        }
        0
    }

    fn sample_host_and_processes(&mut self, now: u64) -> Result<(), BpfError> {
        let elapsed_ns = now.saturating_sub(self.last_sample_ns);
        let elapsed_seconds = (elapsed_ns.max(1) as f64) / 1_000_000_000.0;

        if self.config.has_scope(NetScope::Host) {
            let mut host_delta = StatsValue::default();
            for protocol in [NetProtocol::Tcp, NetProtocol::Udp] {
                if !self.config.has_protocol(protocol) {
                    continue;
                }
                let current = self.read_global(protocol)?;
                let previous = self.global_previous[protocol.index()];
                let delta = current.delta(Some(previous));
                host_delta.accumulate(delta);
                self.global_previous[protocol.index()] = current;
                if self.emit_stats(ScopeKey::Host, protocol, delta, elapsed_seconds, now) != 0 {
                    break;
                }
            }
            if self.config.has_metric(NetMetric::Bytes) {
                let (send_name, recv_name, send_value, recv_value, unit) = if self.config.scaled {
                    (
                        "total_send_rate",
                        "total_recv_rate",
                        host_delta.send_bytes as f64 * 8.0 / elapsed_seconds,
                        host_delta.recv_bytes as f64 * 8.0 / elapsed_seconds,
                        "bits/s",
                    )
                } else {
                    (
                        "total_send",
                        "total_recv",
                        host_delta.send_bytes as f64,
                        host_delta.recv_bytes as f64,
                        "bytes",
                    )
                };
                for sample in [
                    CounterSample {
                        kind: CounterKind::TotalSendBytes,
                        name: send_name.to_string(),
                        value: send_value,
                        unit,
                    },
                    CounterSample {
                        kind: CounterKind::TotalRecvBytes,
                        name: recv_name.to_string(),
                        value: recv_value,
                        unit,
                    },
                ] {
                    if self.emit_sample(ScopeKey::Host, NetProtocol::Tcp, sample, now) != 0 {
                        break;
                    }
                }
            }
        }

        if self.config.has_scope(NetScope::Process) {
            for (key, current) in self.read_processes()? {
                let Some(protocol) = NetProtocol::from_index(key.protocol) else {
                    continue;
                };
                let previous = self.process_previous.insert(key, current);
                let delta = current.delta(previous);
                if self.emit_stats(
                    ScopeKey::Process(key.tgid),
                    protocol,
                    delta,
                    elapsed_seconds,
                    now,
                ) != 0
                {
                    break;
                }
            }
        }

        self.last_sample_ns = now;
        Ok(())
    }

    fn sample_peers(&mut self, now: u64) -> Result<(), BpfError> {
        let elapsed_ns = now.saturating_sub(self.last_peer_sample_ns);
        let elapsed_seconds = (elapsed_ns.max(1) as f64) / 1_000_000_000.0;
        for (key, current) in self.read_peers()? {
            let Some(protocol) = key.protocol() else {
                continue;
            };
            let previous = self.peer_previous.insert(key, current);
            let delta = current.delta(previous);
            if self.emit_stats(ScopeKey::Peer(key), protocol, delta, elapsed_seconds, now) != 0 {
                break;
            }
        }
        self.last_peer_sample_ns = now;
        Ok(())
    }

    fn sample_if_due(&mut self, force: bool) -> Result<(), BpfError> {
        let now = get_monotonic_timestamp();
        let sample_interval = 1_000_000_000u64 / self.config.frequency;
        if force || now.saturating_sub(self.last_sample_ns) >= sample_interval {
            self.sample_host_and_processes(now)?;
        }
        if self.config.has_scope(NetScope::Peer) {
            let peer_interval = 1_000_000_000u64 / self.config.peer_frequency;
            if force || now.saturating_sub(self.last_peer_sample_ns) >= peer_interval {
                self.sample_peers(now)?;
            }
        }
        Ok(())
    }

    pub fn poll(&mut self, _timeout: Duration) -> Result<(), BpfError> {
        self.sample_if_due(false)
    }

    pub fn consume(&mut self) -> Result<(), BpfError> {
        self.sample_if_due(false)
    }

    pub fn flush(&mut self) -> Result<(), BpfError> {
        self.sample_if_due(true)
    }

    pub fn add_pid_filter(&mut self, pid: u32) -> Result<(), BpfError> {
        self.skel
            .maps
            .tracked_tgids
            .update(&pid.to_ne_bytes(), &1u32.to_ne_bytes(), MapFlags::ANY)
            .map_err(|e| BpfError::MapError(format!("failed to add PID filter: {e}")))
    }
}

impl<'this, F> Filterable for NetTrack<'this, F>
where
    F: for<'a> FnMut(Message<'a>) -> i32 + 'this,
{
    fn filter(&mut self, pid: i32) -> Result<(), BpfError> {
        let pid = u32::try_from(pid)
            .map_err(|_| BpfError::MapError(format!("invalid PID filter: {pid}")))?;
        self.add_pid_filter(pid)
    }
}

impl fmt::Display for PeerKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.track_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::rc::Rc;
    use std::thread;

    fn generate_test_traffic() -> (u16, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind TCP listener");
        let tcp_address = listener.local_addr().unwrap();
        let tcp_server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0u8; 4096];
            stream.read_exact(&mut request).unwrap();
            stream.write_all(&vec![1u8; 2048]).unwrap();
        });
        let mut tcp_client = TcpStream::connect(tcp_address).unwrap();
        tcp_client.write_all(&vec![2u8; 4096]).unwrap();
        let mut response = vec![0u8; 2048];
        tcp_client.read_exact(&mut response).unwrap();
        drop(tcp_client);
        tcp_server.join().unwrap();

        let udp_server_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_address = udp_server_socket.local_addr().unwrap();
        let udp_server = thread::spawn(move || {
            let mut request = vec![0u8; 1000];
            let (size, peer) = udp_server_socket.recv_from(&mut request).unwrap();
            assert_eq!(size, request.len());
            assert_eq!(
                udp_server_socket.send_to(&vec![3u8; 500], peer).unwrap(),
                500
            );
        });
        let udp_client = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(
            udp_client.send_to(&vec![4u8; 1000], udp_address).unwrap(),
            1000
        );
        let mut udp_response = vec![0u8; 500];
        assert_eq!(udp_client.recv_from(&mut udp_response).unwrap().0, 500);
        udp_server.join().unwrap();
        (tcp_address.port(), udp_address.port())
    }

    #[test]
    fn defaults_are_low_overhead() {
        let config = NetTrackConfig::default();
        assert_eq!(config.scopes, vec![NetScope::Host]);
        assert_eq!(config.metrics, vec![NetMetric::Bytes]);
        assert_eq!(config.peer_frequency, 1);
    }

    #[test]
    fn cumulative_values_produce_interval_deltas() {
        let previous = StatsValue {
            send_bytes: 100,
            recv_bytes: 40,
            first_seen_ns: 10,
            ..StatsValue::default()
        };
        let current = StatsValue {
            send_bytes: 160,
            recv_bytes: 75,
            first_seen_ns: 10,
            ..StatsValue::default()
        };
        let delta = current.delta(Some(previous));
        assert_eq!(delta.send_bytes, 60);
        assert_eq!(delta.recv_bytes, 35);
    }

    #[test]
    fn changed_generation_uses_current_value() {
        let previous = StatsValue {
            send_bytes: 100,
            first_seen_ns: 10,
            ..StatsValue::default()
        };
        let current = StatsValue {
            send_bytes: 20,
            first_seen_ns: 20,
            ..StatsValue::default()
        };
        assert_eq!(current.delta(Some(previous)).send_bytes, 20);
    }

    #[test]
    fn peer_names_format_ipv4_and_ipv6() {
        let ipv4 = PeerKey {
            protocol: TCP_INDEX as u8,
            family: AF_INET,
            remote_port: 443,
            remote_addr: [127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ..PeerKey::default()
        };
        assert_eq!(ipv4.track_name(), "tcp 127.0.0.1:443");

        let ipv6 = PeerKey {
            protocol: UDP_INDEX as u8,
            family: AF_INET6,
            remote_port: 53,
            remote_addr: Ipv6Addr::LOCALHOST.octets(),
            ..PeerKey::default()
        };
        assert_eq!(ipv6.track_name(), "udp [::1]:53");
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let config = NetTrackConfig {
            frequency: 0,
            ..NetTrackConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    #[ignore = "requires root"]
    fn root_tests_exact_tcp_and_udp_payload_bytes() {
        assert_eq!(unsafe { libc::geteuid() }, 0);

        let counters = Rc::new(RefCell::new(HashMap::<String, f64>::new()));
        let callback_counters = counters.clone();
        let callback = move |message: Message<'_>| {
            if let Message::Event(Event::Counter(counter)) = message {
                callback_counters
                    .borrow_mut()
                    .insert(counter.name.to_string(), counter.value);
            }
            0
        };
        let config = NetTrackConfig {
            scopes: vec![NetScope::Host],
            metrics: vec![NetMetric::Bytes],
            pid_filters: vec![unsafe { libc::getpid() }],
            ..NetTrackConfig::default()
        };
        let mut object = Object::new(config);
        let mut tracker = object.build(callback).expect("failed to load nettrack");

        generate_test_traffic();

        tracker.flush().expect("failed to sample network maps");
        let counters = counters.borrow();
        assert_eq!(counters.get("tcp_send"), Some(&6144.0));
        assert_eq!(counters.get("tcp_recv"), Some(&6144.0));
        assert_eq!(counters.get("udp_send"), Some(&1500.0));
        assert_eq!(counters.get("udp_recv"), Some(&1500.0));
        assert_eq!(counters.get("total_send"), Some(&7644.0));
        assert_eq!(counters.get("total_recv"), Some(&7644.0));
    }

    #[derive(Default)]
    struct ScopedCapture {
        process_counter_ids: HashSet<TrackId>,
        process_counters: HashMap<String, f64>,
        peer_tracks: Vec<String>,
    }

    #[test]
    #[ignore = "requires root"]
    fn root_tests_process_and_peer_scopes() {
        assert_eq!(unsafe { libc::geteuid() }, 0);

        let capture = Rc::new(RefCell::new(ScopedCapture::default()));
        let callback_capture = capture.clone();
        let callback = move |message: Message<'_>| {
            let mut capture = callback_capture.borrow_mut();
            if let Message::Event(event) = message {
                match event {
                    Event::Track(track) => match track.track_type {
                        TrackType::Custom { .. }
                            if matches!(track.parent, Some(TrackType::Process { .. })) =>
                        {
                            capture.peer_tracks.push(track.name.to_string());
                        }
                        TrackType::Counter { id, .. }
                            if matches!(track.parent, Some(TrackType::Process { .. })) =>
                        {
                            capture.process_counter_ids.insert(TrackId::Counter { id });
                        }
                        _ => {}
                    },
                    Event::Counter(counter)
                        if capture.process_counter_ids.contains(&counter.track_id) =>
                    {
                        capture
                            .process_counters
                            .insert(counter.name.to_string(), counter.value);
                    }
                    _ => {}
                }
            }
            0
        };
        let config = NetTrackConfig {
            scopes: vec![NetScope::Process, NetScope::Peer],
            metrics: vec![NetMetric::Bytes],
            pid_filters: vec![unsafe { libc::getpid() }],
            max_peer_tracks: 16,
            ..NetTrackConfig::default()
        };
        let mut object = Object::new(config);
        let mut tracker = object.build(callback).expect("failed to load nettrack");
        let (tcp_port, udp_port) = generate_test_traffic();
        tracker.flush().expect("failed to sample network maps");

        let capture = capture.borrow();
        assert_eq!(capture.process_counters.get("tcp_send"), Some(&6144.0));
        assert_eq!(capture.process_counters.get("tcp_recv"), Some(&6144.0));
        assert_eq!(capture.process_counters.get("udp_send"), Some(&1500.0));
        assert_eq!(capture.process_counters.get("udp_recv"), Some(&1500.0));
        assert!(
            capture
                .peer_tracks
                .contains(&format!("tcp 127.0.0.1:{tcp_port}")),
            "missing TCP peer track: {:?}",
            capture.peer_tracks
        );
        assert!(
            capture
                .peer_tracks
                .contains(&format!("udp 127.0.0.1:{udp_port}")),
            "missing UDP peer track: {:?}",
            capture.peer_tracks
        );
    }
}
