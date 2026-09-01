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

use agent::AgentBuilder;
use clap::Parser;
use std::hint::black_box;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, info_span, instrument, warn};
use tracing_manytrace::{ManytraceLayer, TracingExtension};
use tracing_subscriber::prelude::*;

const TCP_PAYLOAD_BYTES: usize = 32 * 1024;
const UDP_PAYLOAD_BYTES: usize = 1200;
const TICK: Duration = Duration::from_millis(100);

#[derive(Parser)]
#[command(about = "Instrumented TCP/UDP workload for a manytrace demo")]
struct Args {
    #[arg(help = "Path to the manytrace agent socket")]
    socket_path: String,

    #[arg(default_value_t = 12, help = "Workload duration in seconds")]
    seconds: u64,
}

#[inline(never)]
fn cpu_burn(seed: u64, iterations: usize) -> u64 {
    let mut value = seed | 1;
    for i in 0..iterations {
        value = value
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(i as u64 ^ value.rotate_left(17));
    }
    black_box(value)
}

fn phase(elapsed: Duration) -> (&'static str, usize, usize) {
    match elapsed.as_secs() % 4 {
        0 => ("warmup", 1, 2),
        1 => ("medium", 4, 8),
        2 => ("peak", 12, 24),
        _ => ("cooldown", 2, 5),
    }
}

#[instrument(skip(listener))]
fn tcp_server(listener: TcpListener) -> io::Result<()> {
    let (mut stream, peer) = listener.accept()?;
    info!(%peer, "TCP peer accepted");
    let mut buffer = vec![0u8; TCP_PAYLOAD_BYTES];

    loop {
        match stream.read_exact(&mut buffer) {
            Ok(()) => stream.write_all(&buffer)?,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        }
    }

    info!("TCP server observed client EOF");
    Ok(())
}

#[instrument(skip(stream))]
fn tcp_client(mut stream: TcpStream, duration: Duration) -> io::Result<()> {
    let mut probe = [0u8; 1];
    stream.set_nonblocking(true)?;
    if let Err(error) = stream.read(&mut probe) {
        info!(kind = ?error.kind(), "expected nonblocking TCP receive error");
    }
    stream.set_nonblocking(false)?;

    let payload = vec![0x5a; TCP_PAYLOAD_BYTES];
    let mut echo = vec![0u8; TCP_PAYLOAD_BYTES];
    let started = Instant::now();
    let deadline = started + duration;

    while Instant::now() < deadline {
        let tick_started = Instant::now();
        let elapsed = tick_started.duration_since(started);
        let (phase_name, tcp_bursts, _) = phase(elapsed);
        let span = info_span!(
            "tcp_burst",
            phase = phase_name,
            bursts = tcp_bursts,
            payload_bytes = TCP_PAYLOAD_BYTES
        );
        let _entered = span.enter();
        for _ in 0..tcp_bursts {
            stream.write_all(&payload)?;
            stream.read_exact(&mut echo)?;
        }
        black_box(cpu_burn(elapsed.as_nanos() as u64, 80_000));
        if let Some(remaining) = TICK.checked_sub(tick_started.elapsed()) {
            thread::sleep(remaining);
        }
    }

    stream.shutdown(Shutdown::Write)?;
    let eof_bytes = stream.read(&mut probe)?;
    info!(eof_bytes, "TCP client observed server EOF");
    Ok(())
}

#[instrument(skip(socket, running))]
fn udp_server(socket: UdpSocket, running: Arc<AtomicBool>) -> io::Result<()> {
    socket.set_read_timeout(Some(TICK))?;
    let mut buffer = [0u8; UDP_PAYLOAD_BYTES];
    while running.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buffer) {
            Ok((size, peer)) => {
                socket.send_to(&buffer[..size], peer)?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[instrument(skip(socket))]
fn udp_client(socket: UdpSocket, peer: std::net::SocketAddr, duration: Duration) -> io::Result<()> {
    socket.connect(peer)?;
    let mut probe = [0u8; UDP_PAYLOAD_BYTES];
    socket.set_nonblocking(true)?;
    if let Err(error) = socket.recv(&mut probe) {
        info!(kind = ?error.kind(), "expected nonblocking UDP receive error");
    }
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;

    if let Err(error) = socket.send(&vec![0u8; 70_000]) {
        info!(kind = ?error.kind(), "expected oversized UDP send error");
    }

    let payload = [0xa5; UDP_PAYLOAD_BYTES];
    let started = Instant::now();
    let deadline = started + duration;
    while Instant::now() < deadline {
        let tick_started = Instant::now();
        let elapsed = tick_started.duration_since(started);
        let (phase_name, _, udp_datagrams) = phase(elapsed);
        let span = info_span!(
            "udp_burst",
            phase = phase_name,
            datagrams = udp_datagrams,
            payload_bytes = UDP_PAYLOAD_BYTES
        );
        let _entered = span.enter();
        for _ in 0..udp_datagrams {
            socket.send(&payload)?;
            let received = socket.recv(&mut probe)?;
            if received != payload.len() {
                warn!(received, expected = payload.len(), "short UDP echo");
            }
        }
        if let Some(remaining) = TICK.checked_sub(tick_started.elapsed()) {
            thread::sleep(remaining);
        }
    }
    Ok(())
}

#[instrument]
fn cpu_worker(duration: Duration) {
    let started = Instant::now();
    let deadline = started + duration;
    let mut checksum = 1u64;
    while Instant::now() < deadline {
        let span = info_span!("compute_batch", iteration = checksum & 0xff);
        let _entered = span.enter();
        checksum ^= cpu_burn(checksum, 1_500_000);
        thread::yield_now();
    }
    info!(checksum, "CPU worker complete");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let extension = Arc::new(TracingExtension::new());
    let _agent = AgentBuilder::new(args.socket_path)
        .register_tracing(Box::new((*extension).clone()))
        .build()?;

    tracing_subscriber::registry()
        .with(ManytraceLayer::new(extension.clone()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("waiting for manytrace to connect");
    while !extension.is_active() {
        thread::sleep(Duration::from_millis(50));
    }

    let duration = Duration::from_secs(args.seconds);
    info!(seconds = args.seconds, "starting network workload");

    let tcp_listener = TcpListener::bind("127.0.0.1:0")?;
    let tcp_address = tcp_listener.local_addr()?;
    let udp_server_socket = UdpSocket::bind("127.0.0.1:0")?;
    let udp_address = udp_server_socket.local_addr()?;
    let udp_client_socket = UdpSocket::bind("127.0.0.1:0")?;
    let udp_running = Arc::new(AtomicBool::new(true));

    let tcp_server_handle = thread::Builder::new()
        .name("tcp-server".to_string())
        .spawn(move || tcp_server(tcp_listener))?;
    let tcp_client_handle = thread::Builder::new()
        .name("tcp-client".to_string())
        .spawn(move || tcp_client(TcpStream::connect(tcp_address)?, duration))?;
    let udp_running_server = udp_running.clone();
    let udp_server_handle = thread::Builder::new()
        .name("udp-server".to_string())
        .spawn(move || udp_server(udp_server_socket, udp_running_server))?;
    let udp_client_handle = thread::Builder::new()
        .name("udp-client".to_string())
        .spawn(move || udp_client(udp_client_socket, udp_address, duration))?;
    let cpu_handle = thread::Builder::new()
        .name("cpu-worker".to_string())
        .spawn(move || cpu_worker(duration))?;

    tcp_client_handle.join().expect("TCP client panicked")?;
    udp_client_handle.join().expect("UDP client panicked")?;
    udp_running.store(false, Ordering::Relaxed);
    tcp_server_handle.join().expect("TCP server panicked")?;
    udp_server_handle.join().expect("UDP server panicked")?;
    cpu_handle.join().expect("CPU worker panicked");

    info!("network workload complete");
    thread::sleep(Duration::from_millis(250));
    Ok(())
}
