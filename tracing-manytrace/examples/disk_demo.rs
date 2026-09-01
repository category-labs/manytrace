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
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, info_span, instrument};
use tracing_manytrace::{ManytraceLayer, TracingExtension};
use tracing_subscriber::prelude::*;

const FILE_BYTES: u64 = 128 * 1024 * 1024;
const IO_BYTES: usize = 128 * 1024;
const ALIGNMENT: usize = 4096;
const BATCH_OPERATIONS: usize = 8;

#[derive(Parser)]
#[command(about = "Instrumented direct-I/O workload for a manytrace block profile")]
struct Args {
    #[arg(help = "Path to the manytrace agent socket")]
    socket_path: String,

    #[arg(help = "File used for direct I/O")]
    file_path: PathBuf,

    #[arg(default_value_t = 10, help = "Workload duration in seconds")]
    seconds: u64,

    #[arg(default_value_t = 4, help = "Maximum concurrent I/O workers")]
    workers: usize,
}

struct AlignedBuffer {
    pointer: NonNull<u8>,
    len: usize,
}

impl AlignedBuffer {
    fn new(len: usize, fill: u8) -> io::Result<Self> {
        let mut pointer = std::ptr::null_mut();
        let result = unsafe { libc::posix_memalign(&mut pointer, ALIGNMENT, len) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        let pointer = NonNull::new(pointer.cast::<u8>())
            .ok_or_else(|| io::Error::other("posix_memalign returned null"))?;
        unsafe { std::ptr::write_bytes(pointer.as_ptr(), fill, len) };
        Ok(Self { pointer, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        unsafe { libc::free(self.pointer.as_ptr().cast()) };
    }
}

fn write_all_at(file: &File, mut buffer: &[u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        let written = file.write_at(buffer, offset)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short direct write",
            ));
        }
        buffer = &buffer[written..];
        offset += written as u64;
    }
    Ok(())
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buffer.is_empty() {
        let read = file.read_at(buffer, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short direct read",
            ));
        }
        buffer = &mut buffer[read..];
        offset += read as u64;
    }
    Ok(())
}

fn phase(elapsed: Duration, workers: usize) -> (&'static str, usize, bool, Duration) {
    match elapsed.as_secs() % 8 {
        0..=1 => ("write_ramp", 1, true, Duration::from_millis(4)),
        2..=4 => (
            "mixed_saturation",
            workers,
            elapsed.as_millis() % 2 == 0,
            Duration::ZERO,
        ),
        5..=6 => ("parallel_reads", workers, false, Duration::from_millis(1)),
        _ => ("cooldown", 1, true, Duration::from_millis(8)),
    }
}

#[instrument(skip(file))]
fn io_worker(id: usize, workers: usize, file: Arc<File>, duration: Duration) -> io::Result<()> {
    let mut write_buffer = AlignedBuffer::new(IO_BYTES, 0x40 + id as u8)?;
    let mut read_buffer = AlignedBuffer::new(IO_BYTES, 0)?;
    let started = Instant::now();
    let deadline = started + duration;
    let slots = (FILE_BYTES / IO_BYTES as u64) as usize;
    let mut sequence = id.wrapping_mul(104_729).wrapping_add(1);

    while Instant::now() < deadline {
        let elapsed = started.elapsed();
        let (phase_name, active_workers, write, delay) = phase(elapsed, workers);
        if id >= active_workers {
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        let span = info_span!(
            "disk_io_batch",
            worker = id,
            phase = phase_name,
            operation = if write { "write" } else { "read" },
            operations = BATCH_OPERATIONS,
            bytes_per_operation = IO_BYTES
        );
        let _entered = span.enter();
        for _ in 0..BATCH_OPERATIONS {
            sequence = sequence.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let slot = sequence % slots;
            let offset = (slot * IO_BYTES) as u64;
            if write {
                write_buffer.as_mut_slice()[0] = sequence as u8;
                write_all_at(&file, write_buffer.as_slice(), offset)?;
            } else {
                read_exact_at(&file, read_buffer.as_mut_slice(), offset)?;
            }
        }
        if !delay.is_zero() {
            thread::sleep(delay);
        }
    }
    info!(worker = id, "I/O worker complete");
    Ok(())
}

#[instrument(skip(file))]
fn initialize_file(file: &File) -> io::Result<()> {
    let buffer = AlignedBuffer::new(IO_BYTES, 0x5a)?;
    for offset in (0..FILE_BYTES).step_by(IO_BYTES) {
        write_all_at(file, buffer.as_slice(), offset)?;
    }
    file.sync_all()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.workers == 0 {
        return Err("workers must be greater than zero".into());
    }
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

    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&args.file_path)?;
    file.set_len(FILE_BYTES)?;
    info!(path = %args.file_path.display(), bytes = FILE_BYTES, "initializing direct-I/O file");
    initialize_file(&file)?;

    let file = Arc::new(file);
    let duration = Duration::from_secs(args.seconds);
    let mut handles = Vec::new();
    for id in 0..args.workers {
        let file = file.clone();
        let workers = args.workers;
        handles.push(
            thread::Builder::new()
                .name(format!("disk-io-{id}"))
                .spawn(move || io_worker(id, workers, file, duration))?,
        );
    }
    for handle in handles {
        handle.join().expect("disk I/O worker panicked")?;
    }
    file.sync_all()?;
    info!("disk workload complete");
    thread::sleep(Duration::from_millis(250));
    Ok(())
}
