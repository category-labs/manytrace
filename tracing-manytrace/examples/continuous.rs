use agent::AgentBuilder;
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, instrument, warn};
use tracing_manytrace::{ManytraceLayer, TracingExtension};
use tracing_subscriber::prelude::*;

#[derive(Parser)]
#[command(name = "continuous")]
#[command(about = "Continuous tracing example with graceful shutdown")]
struct Args {
    #[arg(help = "Path to the manytrace agent socket")]
    socket_path: String,
}

#[instrument]
fn worker_loop(id: u32, running: Arc<AtomicBool>) {
    info!(worker_id = id, "worker thread started");
    let mut iteration = 0u64;

    while running.load(Ordering::Relaxed) {
        iteration += 1;
        process_work(id, iteration);
        thread::sleep(Duration::from_millis(100));
    }

    info!(
        worker_id = id,
        iterations = iteration,
        "worker thread stopping"
    );
}

#[instrument]
fn process_work(worker_id: u32, iteration: u64) {
    debug!(worker_id, iteration, "processing work item");

    if iteration % 10 == 0 {
        info!(worker_id, iteration, "milestone reached");
    }

    if iteration % 25 == 0 {
        warn!(worker_id, iteration, "periodic maintenance required");
        perform_maintenance(worker_id);
    }

    thread::sleep(Duration::from_millis(50));
}

#[instrument]
fn perform_maintenance(worker_id: u32) {
    debug!(worker_id, "performing maintenance");
    thread::sleep(Duration::from_millis(200));
}

#[instrument]
fn monitoring_thread(running: Arc<AtomicBool>) {
    info!("monitoring thread started");
    let mut checks = 0u64;

    while running.load(Ordering::Relaxed) {
        checks += 1;
        debug!(checks, "performing system check");

        if checks % 5 == 0 {
            info!(checks, "system health check");
        }

        thread::sleep(Duration::from_secs(1));
    }

    info!(total_checks = checks, "monitoring thread stopping");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let extension = Arc::new(TracingExtension::new());
    let _agent = AgentBuilder::new(args.socket_path.clone())
        .register_tracing(Box::new((*extension).clone()))
        .build()?;

    tracing_subscriber::registry()
        .with(ManytraceLayer::new(extension))
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("starting continuous tracing example");

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        info!("received ctrl-c signal");
        r.store(false, Ordering::Relaxed);
    })?;

    let mut handles = vec![];

    for i in 0..4 {
        let running_clone = running.clone();
        let handle = thread::Builder::new()
            .name(format!("worker-{}", i))
            .spawn(move || {
                worker_loop(i, running_clone);
            })?;
        handles.push(handle);
    }

    let running_clone = running.clone();
    let monitor_handle = thread::Builder::new()
        .name("monitor".to_string())
        .spawn(move || {
            monitoring_thread(running_clone);
        })?;
    handles.push(monitor_handle);

    info!("all threads started, press ctrl-c to stop");

    for handle in handles {
        handle.join().unwrap();
    }

    info!("all threads terminated gracefully");
    thread::sleep(Duration::from_millis(100));

    Ok(())
}
