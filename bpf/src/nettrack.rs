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

use crate::{perf_event, BpfError, Filterable};
use libbpf_rs::skel::{OpenSkel, Skel, SkelBuilder};
use libbpf_rs::{OpenObject, RingBufferBuilder};
use libbpf_sys::{PERF_COUNT_SW_CPU_CLOCK, PERF_TYPE_SOFTWARE};
use protocol::{Counter, Event, Labels, Message, Track, TrackId, TrackType};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::time::Duration;
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetTrackConfig {
    #[serde(default = "default_frequency")]
    pub frequency: u64,
    #[serde(default = "default_ringbuf_size")]
    pub ringbuf: usize,
    #[serde(default)]
    pub scaled: bool,
}

fn default_frequency() -> u64 {
    9
}

fn default_ringbuf_size() -> usize {
    1024 * 1024
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
        let nettrack = NetTrack::new(
            &mut self.object,
            self.config.clone(),
            callback,
            self.config.frequency,
        )?;
        Ok(nettrack)
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct NetEvent {
    pub timestamp: u64,
    pub tcp_send_bytes: u64,
    pub tcp_recv_bytes: u64,
    pub udp_send_bytes: u64,
    pub udp_recv_bytes: u64,
    pub total_send_bytes: u64,
    pub total_recv_bytes: u64,
    pub cpu: u32,
}

unsafe impl plain::Plain for NetEvent {}

#[derive(Debug)]
struct CounterIds {
    tcp_send: u64,
    tcp_recv: u64,
    udp_send: u64,
    udp_recv: u64,
    total_send: u64,
    total_recv: u64,
}

impl CounterIds {
    fn new() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            tcp_send: rng.gen(),
            tcp_recv: rng.gen(),
            udp_send: rng.gen(),
            udp_recv: rng.gen(),
            total_send: rng.gen(),
            total_recv: rng.gen(),
        }
    }
}

#[derive(Default, Clone)]
struct NetStats {
    tcp_send_total: u64,
    tcp_recv_total: u64,
    udp_send_total: u64,
    udp_recv_total: u64,
    last_timestamp: u64,
}

impl NetStats {
    fn accumulate(&mut self, event: &NetEvent) {
        self.tcp_send_total += event.tcp_send_bytes;
        self.tcp_recv_total += event.tcp_recv_bytes;
        self.udp_send_total += event.udp_send_bytes;
        self.udp_recv_total += event.udp_recv_bytes;
        self.last_timestamp = event.timestamp;
    }

    fn reset(&mut self) {
        self.tcp_send_total = 0;
        self.tcp_recv_total = 0;
        self.udp_send_total = 0;
        self.udp_recv_total = 0;
    }

    fn total_send(&self) -> u64 {
        self.tcp_send_total + self.udp_send_total
    }

    fn total_recv(&self) -> u64 {
        self.tcp_recv_total + self.udp_recv_total
    }
}

impl<'a> TryFrom<&'a [u8]> for &'a NetEvent {
    type Error = BpfError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        plain::from_bytes(data)
            .map_err(|e| BpfError::MapError(format!("failed to parse net event: {:?}", e)))
    }
}

pub struct NetTrack<'this, F> {
    #[allow(dead_code)]
    skel: NettrackSkel<'this>,
    ringbuf: libbpf_rs::RingBuffer<'this>,
    _perf_links: Vec<libbpf_rs::Link>,
    _phantom: PhantomData<F>,
}

impl<'this, F> NetTrack<'this, F>
where
    F: for<'a> FnMut(Message<'a>) -> i32 + 'this,
{
    fn load_and_attach_skel(
        open_object: &'this mut MaybeUninit<OpenObject>,
        config: &NetTrackConfig,
    ) -> Result<NettrackSkel<'this>, BpfError> {
        let skel_builder = NettrackSkelBuilder::default();

        let mut open_skel = skel_builder
            .open(open_object)
            .map_err(|e| BpfError::LoadError(format!("failed to open bpf skeleton: {}", e)))?;

        open_skel
            .maps
            .events
            .set_max_entries(config.ringbuf as u32)
            .map_err(|e| BpfError::LoadError(format!("failed to set ring buffer size: {}", e)))?;

        let mut skel = open_skel
            .load()
            .map_err(|e| BpfError::LoadError(format!("failed to load bpf program: {}", e)))?;

        skel.attach()
            .map_err(|e| BpfError::AttachError(format!("failed to attach bpf programs: {}", e)))?;

        Ok(skel)
    }

    fn setup_perf_events(
        skel: &mut NettrackSkel<'this>,
        freq: u64,
    ) -> Result<Vec<libbpf_rs::Link>, BpfError> {
        let perf_fds =
            perf_event::perf_event_per_cpu(PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK, freq)
                .map_err(|e| BpfError::LoadError(format!("failed to open perf events: {}", e)))?;

        let prog = &mut skel.progs.handle_boundary_event;
        let links = perf_event::attach_perf_event(&perf_fds, prog)
            .map_err(|e| BpfError::AttachError(format!("failed to attach perf events: {}", e)))?;

        Ok(links)
    }

    fn new(
        open_object: &'this mut MaybeUninit<OpenObject>,
        config: NetTrackConfig,
        callback: F,
        freq: u64,
    ) -> Result<Self, BpfError> {
        let mut skel = Self::load_and_attach_skel(open_object, &config)?;
        let counter_ids = CounterIds::new();
        let ringbuf = Self::create_ring_buffer(&skel, callback, counter_ids, config.scaled)?;
        let perf_links = Self::setup_perf_events(&mut skel, freq)?;

        Ok(NetTrack {
            skel,
            ringbuf,
            _perf_links: perf_links,
            _phantom: PhantomData,
        })
    }

    fn create_counters<'a>(
        stats: &NetStats,
        ids: &CounterIds,
        scaled: bool,
        time_diff_s: Option<f64>,
    ) -> Vec<(&'a str, f64, u64)> {
        if scaled {
            let time_diff = time_diff_s.unwrap_or(1.0);
            vec![
                (
                    "tcp_send_rate",
                    (stats.tcp_send_total as f64 * 8.0) / time_diff,
                    ids.tcp_send,
                ),
                (
                    "tcp_recv_rate",
                    (stats.tcp_recv_total as f64 * 8.0) / time_diff,
                    ids.tcp_recv,
                ),
                (
                    "udp_send_rate",
                    (stats.udp_send_total as f64 * 8.0) / time_diff,
                    ids.udp_send,
                ),
                (
                    "udp_recv_rate",
                    (stats.udp_recv_total as f64 * 8.0) / time_diff,
                    ids.udp_recv,
                ),
                (
                    "total_send_rate",
                    (stats.total_send() as f64 * 8.0) / time_diff,
                    ids.total_send,
                ),
                (
                    "total_recv_rate",
                    (stats.total_recv() as f64 * 8.0) / time_diff,
                    ids.total_recv,
                ),
            ]
        } else {
            vec![
                ("tcp_send", stats.tcp_send_total as f64, ids.tcp_send),
                ("tcp_recv", stats.tcp_recv_total as f64, ids.tcp_recv),
                ("udp_send", stats.udp_send_total as f64, ids.udp_send),
                ("udp_recv", stats.udp_recv_total as f64, ids.udp_recv),
                ("total_send", stats.total_send() as f64, ids.total_send),
                ("total_recv", stats.total_recv() as f64, ids.total_recv),
            ]
        }
    }

    fn submit_counter<G>(
        name: &str,
        value: f64,
        id: u64,
        timestamp: u64,
        submitted_tracks: &mut HashMap<TrackId, ()>,
        callback: &mut G,
        unit: &str,
    ) -> i32
    where
        G: FnMut(Message) -> i32,
    {
        let track_id = TrackId::Counter { id };

        if submitted_tracks.insert(track_id, ()).is_none() {
            let track = Message::Event(Event::Track(Track {
                name,
                track_type: TrackType::Counter {
                    id,
                    unit: Some(unit),
                },
                parent: None,
            }));
            let result = callback(track);
            if result != 0 {
                return result;
            }
        }

        debug!(
            name = name,
            value = value,
            timestamp = timestamp,
            "emitting network counter"
        );

        callback(Message::Event(Event::Counter(Counter {
            name,
            value,
            timestamp,
            track_id,
            labels: Cow::Owned(Labels::new()),
            unit: Some(unit),
        })))
    }

    fn create_ring_buffer(
        skel: &NettrackSkel<'this>,
        mut callback: F,
        counter_ids: CounterIds,
        scaled: bool,
    ) -> Result<libbpf_rs::RingBuffer<'this>, BpfError> {
        let mut submitted_tracks: HashMap<TrackId, ()> = HashMap::new();

        let mut builder = RingBufferBuilder::new();
        let nprocs = libbpf_rs::num_possible_cpus().unwrap();
        let mut boundaries_reported = 0;
        let mut stats = NetStats::default();
        let mut prev_timestamp = 0u64;

        builder
            .add(&skel.maps.events, move |data| {
                let net_event: &NetEvent = data.try_into().unwrap();
                stats.accumulate(net_event);
                boundaries_reported += 1;

                if boundaries_reported == nprocs {
                    boundaries_reported = 0;

                    if scaled && prev_timestamp == 0 {
                        prev_timestamp = stats.last_timestamp;
                        stats.reset();
                        return 0;
                    }

                    let time_diff_s = if scaled && prev_timestamp > 0 {
                        let time_diff_ns = stats.last_timestamp.saturating_sub(prev_timestamp);
                        if time_diff_ns > 0 {
                            Some(time_diff_ns as f64 / 1_000_000_000.0)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let counters = Self::create_counters(&stats, &counter_ids, scaled, time_diff_s);
                    let unit = if scaled { "bits/s" } else { "bytes" };

                    for (name, value, id) in counters {
                        let result = Self::submit_counter(
                            name,
                            value,
                            id,
                            stats.last_timestamp,
                            &mut submitted_tracks,
                            &mut callback,
                            unit,
                        );
                        if result != 0 {
                            return result;
                        }
                    }

                    prev_timestamp = stats.last_timestamp;
                    stats.reset();
                }

                0
            })
            .map_err(|e| BpfError::MapError(format!("failed to add ring buffer: {}", e)))?;

        builder
            .build()
            .map_err(|e| BpfError::MapError(format!("failed to build ring buffer: {}", e)))
    }

    pub fn poll(&mut self, timeout: Duration) -> Result<(), BpfError> {
        self.ringbuf
            .poll(timeout)
            .map_err(|e| BpfError::MapError(format!("failed to poll ring buffer: {}", e)))?;
        Ok(())
    }

    pub fn consume(&mut self) -> Result<(), BpfError> {
        self.ringbuf
            .consume()
            .map_err(|e| BpfError::MapError(format!("failed to consume ring buffer: {}", e)))?;
        Ok(())
    }
}

impl<'this, F> Filterable for NetTrack<'this, F>
where
    F: for<'a> FnMut(Message<'a>) -> i32 + 'this,
{
    fn filter(&mut self, _pid: i32) -> Result<(), BpfError> {
        Ok(())
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use rstest::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::{
        cell::RefCell,
        rc::Rc,
        thread,
        time::{Duration, Instant},
    };

    fn is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[derive(Debug, Clone)]
    struct TestCounter {
        name: String,
        value: f64,
        #[allow(dead_code)]
        timestamp: u64,
    }

    struct NetTrackFixture {
        events: Rc<RefCell<Vec<TestCounter>>>,
    }

    #[fixture]
    fn nettrack_setup() -> NetTrackFixture {
        let events = Rc::new(RefCell::new(Vec::new()));

        NetTrackFixture { events }
    }

    fn create_test_callback(events: Rc<RefCell<Vec<TestCounter>>>) -> impl FnMut(Message) -> i32 {
        move |message| {
            if let Message::Event(Event::Counter(c)) = message {
                let test_counter = TestCounter {
                    name: c.name.to_string(),
                    value: c.value,
                    timestamp: c.timestamp,
                };
                events.borrow_mut().push(test_counter);
            }
            0
        }
    }

    fn start_tcp_server(listener: TcpListener) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("failed to accept connection");
            let mut buf = [0; 1024];
            while stream.read(&mut buf).unwrap_or(0) > 0 {
                stream
                    .write_all(b"response")
                    .expect("failed to write response");
            }
        })
    }

    fn start_udp_server(socket: UdpSocket) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut buf = [0; 1024];
            loop {
                if let Ok((size, src)) = socket.recv_from(&mut buf) {
                    if size == 0 {
                        break;
                    }
                    socket
                        .send_to(&buf[..size], src)
                        .expect("failed to send UDP response");
                }
            }
        })
    }

    fn generate_network_traffic<'b, F>(
        tcp_addr: std::net::SocketAddr,
        udp_addr: std::net::SocketAddr,
        duration: Duration,
        nettrack: &mut NetTrack<'b, F>,
    ) where
        F: for<'a> FnMut(Message<'a>) -> i32 + 'b,
    {
        let start = Instant::now();
        let data = vec![b'x'; 1024];

        while start.elapsed() < duration {
            if let Ok(mut tcp_stream) = TcpStream::connect(tcp_addr) {
                let _ = tcp_stream.write_all(&data);
                let mut response = [0; 1024];
                let _ = tcp_stream.read(&mut response);
            }

            let udp_client = UdpSocket::bind("127.0.0.1:0").expect("failed to bind UDP client");
            udp_client
                .send_to(&data, udp_addr)
                .expect("failed to send UDP data");
            let mut response = [0; 1024];
            let _ = udp_client.recv(&mut response);

            let _ = nettrack.consume();
        }
    }

    fn verify_events(events: &[TestCounter]) {
        let event_types = [
            "tcp_send",
            "tcp_recv",
            "udp_send",
            "udp_recv",
            "total_send",
            "total_recv",
        ];

        for event_type in &event_types {
            let type_events: Vec<_> = events.iter().filter(|c| c.name == *event_type).collect();
            assert!(!type_events.is_empty(), "no {} events", event_type);
        }

        for event in events {
            assert!(event.value >= 0.0, "bytes should not be negative");
        }
    }

    #[rstest]
    #[ignore = "requires root"]
    fn test_nettrack_tcp_udp_rates(nettrack_setup: NetTrackFixture) {
        assert!(is_root());

        let config = NetTrackConfig {
            frequency: 100,
            ringbuf: default_ringbuf_size(),
            scaled: false,
        };

        let mut object = Object::new(config);
        let callback = create_test_callback(nettrack_setup.events.clone());
        let mut nettrack = object.build(callback).expect("failed to build nettrack");

        let tcp_listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind TCP listener");
        let tcp_addr = tcp_listener
            .local_addr()
            .expect("failed to get TCP address");

        let udp_socket = UdpSocket::bind("127.0.0.1:0").expect("failed to bind UDP socket");
        let udp_addr = udp_socket.local_addr().expect("failed to get UDP address");

        let tcp_thread = start_tcp_server(tcp_listener);
        let udp_thread = start_udp_server(udp_socket);

        thread::sleep(Duration::from_millis(100));
        generate_network_traffic(
            tcp_addr,
            udp_addr,
            Duration::from_millis(500),
            &mut nettrack,
        );
        thread::sleep(Duration::from_millis(200));
        let _ = nettrack.consume();

        drop(tcp_thread);
        drop(udp_thread);

        let collected_events = nettrack_setup.events.borrow();
        assert!(!collected_events.is_empty(), "no events were captured");
        verify_events(&collected_events);
    }
}
