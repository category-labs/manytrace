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
            _ => return,
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
    })?;

    while running.load(Ordering::SeqCst) {
        tracker.poll(Duration::from_millis(100))?;
    }

    println!("\nShutting down...");
    Ok(())
}
