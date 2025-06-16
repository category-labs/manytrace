use bpf::{ThreadEvent, ThreadTracker};
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
    println!(
        "{:<8} {:<8} {:<16} {:<40}",
        "PID", "TGID", "COMM", "FILENAME"
    );
    println!("{:-<80}", "");

    let tracker = ThreadTracker::new()?;

    while running.load(Ordering::SeqCst) {
        tracker.poll_events(Duration::from_millis(100), |event: &ThreadEvent| {
            let filename = event.filename_str();
            let filename_display = if filename.is_empty() {
                "-".to_string()
            } else {
                filename.to_string()
            };

            println!(
                "{:<8} {:<8} {:<16} {:<40}",
                event.pid,
                event.tgid,
                event.comm_str(),
                filename_display
            );
        })?;
    }

    println!("\nShutting down...");
    Ok(())
}
