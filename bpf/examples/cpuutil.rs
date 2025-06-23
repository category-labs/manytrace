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
        pid_filters: vec![std::process::id()],
        filter_process: vec![],
    });
    let mut tracker = builder.build(|message: Message| {
        if let Message::Event(Event::Counter(counter)) = message {
            match counter.name {
                "cpu_time_ns" => {
                    println!(
                        "[CPU] PID: {:<8} TID: {:<8} Time: {:<15.0} ns  Timestamp: {} ns",
                        counter.pid, counter.tid, counter.value, counter.timestamp
                    );
                }
                "kernel_time_ns" => {
                    println!(
                        "[KRN] PID: {:<8} TID: {:<8} Time: {:<15.0} ns  Timestamp: {} ns",
                        counter.pid, counter.tid, counter.value, counter.timestamp
                    );
                }
                _ => {}
            }
        }
    })?;

    while running.load(Ordering::SeqCst) {
        tracker.consume()?;
        sleep(Duration::from_millis(100));
    }

    println!("\nShutting down...");
    Ok(())
}
