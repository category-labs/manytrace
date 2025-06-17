use bpf::threadtrack;
use protocol::Event;
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

    let mut builder = threadtrack::Object::new();
    let mut tracker = builder.build(|event: Event| match event {
        Event::ThreadName(thread) => {
            println!(
                "{:<8} {:<8} {:<64} {:<40}",
                thread.pid, thread.tid, thread.name, "Thread"
            );
        }
        Event::ProcessName(process) => {
            println!(
                "{:<8} {:<8} {:<64} {:<40}",
                process.pid, "-", process.name, "Process"
            );
        }
        _ => {}
    })?;

    while running.load(Ordering::SeqCst) {
        tracker.poll(Duration::from_millis(100))?;
    }

    println!("\nShutting down...");
    Ok(())
}
