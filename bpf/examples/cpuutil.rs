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

use bpf::{cpuutil, CpuUtilConfig};
use protocol::{Event, Message};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    println!("Starting CPU tracker. Press Ctrl+C to stop.");
    println!(
        "Tracking CPU utilization for current process (pid: {}) with 1-second intervals",
        std::process::id()
    );
    println!("\nReporting aggregated CPU time per thread every second:\n");

    let mut builder = cpuutil::Object::new(CpuUtilConfig {
        frequency: 1000,
        pid_filters: vec![std::process::id() as i32],
        filter_process: vec![],
        ringbuf: 64 * 1024,
    });
    let mut tracker = builder.build(|message: Message| {
        if let Message::Event(Event::Counter(counter)) = message {
            let (pid, tid) = match &counter.track_id {
                protocol::TrackId::Thread { pid, tid } => (*pid, *tid),
                _ => (0, 0),
            };
            match counter.name {
                "cpu_time" => {
                    println!(
                        "[CPU] PID: {:<8} TID: {:<8} Time: {:<15.2} %  Timestamp: {} ns",
                        pid, tid, counter.value, counter.timestamp
                    );
                }
                "kernel_time" => {
                    println!(
                        "[KRN] PID: {:<8} TID: {:<8} Time: {:<15.2} %  Timestamp: {} ns",
                        pid, tid, counter.value, counter.timestamp
                    );
                }
                _ => {}
            }
        }
        0
    })?;

    while running.load(Ordering::SeqCst) {
        tracker.consume()?;
        sleep(Duration::from_millis(100));
    }

    println!("\nShutting down...");
    Ok(())
}
