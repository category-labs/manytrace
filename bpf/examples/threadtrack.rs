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

use bpf::{threadtrack, ThreadTrackerConfig};
use protocol::{Event, Message};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })?;

    println!("Starting thread tracker. Press Ctrl+C to stop.");
    println!("{:<8} {:<8} {:<16} {:<40}", "PID", "TID", "NAME", "TYPE");
    println!("{:-<80}", "");

    let mut builder = threadtrack::Object::new(ThreadTrackerConfig::default());
    let mut tracker = builder.build(|message: Message| {
        let event = match message {
            Message::Event(e) => e,
            _ => return 0,
        };
        if let Event::Track(track) = event {
            match track.track_type {
                protocol::TrackType::Thread { tid, pid } => {
                    println!("{:<8} {:<8} {:<64} {:<40}", pid, tid, track.name, "Thread");
                }
                protocol::TrackType::Process { pid } => {
                    println!("{:<8} {:<8} {:<64} {:<40}", pid, "-", track.name, "Process");
                }
                _ => {}
            }
        }
        0
    })?;

    while running.load(Ordering::SeqCst) {
        tracker.poll(Duration::from_millis(100))?;
    }

    println!("\nShutting down...");
    Ok(())
}
