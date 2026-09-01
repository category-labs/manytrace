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
use std::io;
use std::mem;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, info_span};
use tracing_manytrace::{ManytraceLayer, TracingExtension};
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[command(about = "Rust workload demonstrating blocked and runnable scheduler states")]
struct Args {
    #[arg(help = "Path to the manytrace agent socket")]
    socket_path: String,

    #[arg(
        default_value_t = 3,
        help = "Duration of each workload phase in seconds"
    )]
    phase_seconds: u64,

    #[arg(default_value_t = 4, help = "CPU contenders pinned to one CPU")]
    contenders: usize,
}

fn first_allowed_cpu() -> io::Result<usize> {
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    let result = unsafe {
        libc::sched_getaffinity(
            0,
            mem::size_of::<libc::cpu_set_t>(),
            &mut set as *mut libc::cpu_set_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }

    (0..libc::CPU_SETSIZE as usize)
        .find(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
        .ok_or_else(|| io::Error::other("process has no allowed CPUs"))
}

fn pin_current_thread(cpu: usize) -> io::Result<()> {
    let mut set: libc::cpu_set_t = unsafe { mem::zeroed() };
    unsafe { libc::CPU_SET(cpu, &mut set) };
    let result = unsafe {
        libc::sched_setaffinity(
            0,
            mem::size_of::<libc::cpu_set_t>(),
            &set as *const libc::cpu_set_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn busy_for(duration: Duration, seed: u64) -> u64 {
    let deadline = Instant::now() + duration;
    let mut value = seed;
    while Instant::now() < deadline {
        value = value
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        black_box(value);
    }
    value
}

fn blocked_worker(cpu: usize, duration: Duration) -> io::Result<()> {
    pin_current_thread(cpu)?;
    let phase = info_span!("blocked_phase", cpu);
    let _phase_guard = phase.enter();
    let deadline = Instant::now() + duration;
    let mut cycle = 0u64;
    let mut checksum = 1u64;

    while Instant::now() < deadline {
        cycle += 1;
        {
            let span = info_span!("intentional_sleep", cycle, sleep_ms = 30);
            let _guard = span.enter();
            thread::sleep(Duration::from_millis(30));
        }
        {
            let span = info_span!("short_cpu_burst", cycle);
            let _guard = span.enter();
            checksum ^= busy_for(Duration::from_millis(5), checksum ^ cycle);
        }
    }

    info!(cycle, checksum, "blocked phase complete");
    Ok(())
}

fn contention_worker(
    id: usize,
    cpu: usize,
    duration: Duration,
    barrier: Arc<Barrier>,
) -> io::Result<()> {
    pin_current_thread(cpu)?;
    barrier.wait();
    let span = info_span!("cpu_contender", worker = id, cpu);
    let _guard = span.enter();
    let checksum = busy_for(duration, id as u64 + 1);
    info!(worker = id, checksum, "CPU contender complete");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.phase_seconds == 0 || args.contenders < 2 {
        return Err("phase_seconds must be positive and contenders must be at least two".into());
    }

    let extension = Arc::new(TracingExtension::new());
    let _agent = AgentBuilder::new(args.socket_path)
        .register_tracing(Box::new((*extension).clone()))
        .build()?;
    tracing_subscriber::registry()
        .with(ManytraceLayer::new(extension.clone()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!(pid = std::process::id(), "waiting for manytrace to connect");
    while !extension.is_active() {
        thread::sleep(Duration::from_millis(50));
    }

    let cpu = first_allowed_cpu()?;
    let phase_duration = Duration::from_secs(args.phase_seconds);
    info!(cpu, "starting scheduler demonstration");

    thread::Builder::new()
        .name("sched-blocked".to_string())
        .spawn(move || blocked_worker(cpu, phase_duration))?
        .join()
        .expect("blocked worker panicked")?;

    thread::sleep(Duration::from_millis(200));

    let barrier = Arc::new(Barrier::new(args.contenders));
    let mut handles = Vec::with_capacity(args.contenders);
    for id in 0..args.contenders {
        let barrier = barrier.clone();
        handles.push(
            thread::Builder::new()
                .name(format!("sched-cpu-{id}"))
                .spawn(move || contention_worker(id, cpu, phase_duration, barrier))?,
        );
    }
    for handle in handles {
        handle.join().expect("CPU contender panicked")?;
    }

    info!("scheduler demonstration complete");
    thread::sleep(Duration::from_millis(250));
    Ok(())
}
