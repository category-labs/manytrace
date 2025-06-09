use clap::Parser;
use hdrhistogram::Histogram;
use mpscbuf::sync::notification::Notification;
use mpscbuf::{Consumer, RingBuf};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use std::collections::HashMap;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

#[derive(Parser, Debug)]
#[clap(name = "consumer")]
#[clap(about = "Ring buffer consumer example", long_about = None)]
struct Args {
    #[clap(short, long, default_value = "/tmp/mpscbuf_producer")]
    socket_prefix: String,

    #[clap(short, long)]
    producer_ids: Vec<u32>,

    #[clap(short, long, default_value_t = 128)]
    buffer_size_mb: usize,

    #[clap(short, long, default_value_t = 10)]
    report_interval_secs: u64,
}

fn connect_to_producer(
    socket_prefix: &str,
    producer_id: u32,
    memory_fd: &std::os::fd::OwnedFd,
    notification_fd: &std::os::fd::OwnedFd,
    memory_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = format!("{}_{}.sock", socket_prefix, producer_id);

    debug!(producer_id = producer_id, socket_path = %socket_path, "connecting to producer");

    let stream = UnixStream::connect(&socket_path)?;
    info!(producer_id = producer_id, socket_path = %socket_path, "connected to producer");

    let size_bytes = memory_size.to_le_bytes();
    let iov = [std::io::IoSlice::new(&size_bytes)];

    let fds = [memory_fd.as_raw_fd(), notification_fd.as_raw_fd()];
    let cmsg = ControlMessage::ScmRights(&fds[..]);

    sendmsg::<()>(
        stream.as_fd().as_raw_fd(),
        &iov,
        &[cmsg],
        MsgFlags::empty(),
        None,
    )?;

    info!(
        producer_id = producer_id,
        memory_fd = memory_fd.as_raw_fd(),
        notification_fd = notification_fd.as_raw_fd(),
        memory_size = memory_size,
        "sent connection info to producer"
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!("starting consumer");
    debug!(?args, "consumer configuration");

    if args.producer_ids.is_empty() {
        error!("must specify at least one producer ID");
        std::process::exit(1);
    }

    let buffer_size = args.buffer_size_mb * 1024 * 1024;
    info!(
        buffer_size_mb = args.buffer_size_mb,
        buffer_size = buffer_size,
        "creating ring buffer"
    );

    let ringbuf = RingBuf::new(buffer_size)?;
    let notification = Notification::new()?;

    let memory_fd = ringbuf.clone_fd()?;
    let notification_fd = notification.fd().try_clone_to_owned()?;

    info!(
        producer_count = args.producer_ids.len(),
        "connecting to producers"
    );
    for &producer_id in &args.producer_ids {
        connect_to_producer(
            &args.socket_prefix,
            producer_id,
            &memory_fd,
            &notification_fd,
            buffer_size,
        )?;
    }

    let consumer = Consumer::new(ringbuf, notification);

    let histogram = Arc::new(Mutex::new(Histogram::<u64>::new(3)?));
    let per_producer_counts = Arc::new(Mutex::new(HashMap::<u32, u64>::new()));

    let histogram_clone = histogram.clone();
    let per_producer_counts_clone = per_producer_counts.clone();
    let report_interval_secs = args.report_interval_secs;

    let consumer_thread = thread::spawn(
        move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let start_time = Instant::now();

            info!("starting message consumption");

            for record_result in consumer.blocking_iter() {
                let record = record_result?;
                let receive_time = start_time.elapsed().as_nanos() as u64;

                let data = record.as_slice();
                debug!(data_len = data.len(), "received record");

                if data.len() >= 24 {
                    let timestamp_bytes: [u8; 8] = data[0..8].try_into().unwrap();
                    let send_time = u64::from_le_bytes(timestamp_bytes);

                    let sequence_bytes: [u8; 8] = data[8..16].try_into().unwrap();
                    let sequence = u64::from_le_bytes(sequence_bytes);

                    let producer_id_bytes: [u8; 4] = data[16..20].try_into().unwrap();
                    let producer_id = u32::from_le_bytes(producer_id_bytes);

                    let latency_ns = receive_time.saturating_sub(send_time);
                    let latency_us = latency_ns / 1000;

                    debug!(
                        producer_id = producer_id,
                        sequence = sequence,
                        latency_us = latency_us,
                        "processed message"
                    );

                    histogram_clone.lock().unwrap().record(latency_us)?;

                    let mut counts = per_producer_counts_clone.lock().unwrap();
                    *counts.entry(producer_id).or_insert(0) += 1;
                } else {
                    warn!(
                        data_len = data.len(),
                        "received message too short, skipping"
                    );
                }
            }

            Ok(())
        },
    );

    info!(
        report_interval_secs = report_interval_secs,
        "starting statistics reporting"
    );

    let mut last_report = Instant::now();
    let mut last_count = 0u64;
    let mut last_producer_counts = HashMap::<u32, u64>::new();

    loop {
        thread::sleep(Duration::from_secs(report_interval_secs));

        let now = Instant::now();
        let elapsed = now.duration_since(last_report);

        let hist = histogram.lock().unwrap();
        let current_count = hist.len();
        let messages_per_sec = (current_count - last_count) as f64 / elapsed.as_secs_f64();

        info!(
            total_rate = format!("{:.2}", messages_per_sec),
            total_messages = current_count,
            elapsed_secs = elapsed.as_secs(),
            "=== statistics report ==="
        );

        let producer_counts = per_producer_counts.lock().unwrap();
        for (producer_id, &count) in producer_counts.iter() {
            let last = last_producer_counts.get(producer_id).copied().unwrap_or(0);
            let rate = (count - last) as f64 / elapsed.as_secs_f64();
            info!(
                producer_id = producer_id,
                rate = format!("{:.2}", rate),
                total_messages = count,
                "producer statistics"
            );
            last_producer_counts.insert(*producer_id, count);
        }

        if !hist.is_empty() {
            info!(
                p50_us = hist.value_at_quantile(0.50),
                p90_us = hist.value_at_quantile(0.90),
                p99_us = hist.value_at_quantile(0.99),
                p99_9_us = hist.value_at_quantile(0.999),
                max_us = hist.max(),
                "latency percentiles"
            );
        }

        last_report = now;
        last_count = current_count;

        if consumer_thread.is_finished() {
            warn!("consumer thread has finished");
            break;
        }
    }

    match consumer_thread.join() {
        Ok(Ok(())) => info!("consumer thread completed successfully"),
        Ok(Err(e)) => error!(error = ?e, "consumer thread failed"),
        Err(_) => error!("consumer thread panicked"),
    }

    Ok(())
}
