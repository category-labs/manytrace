use eyre::{Context, Result};
use perfetto_format::perfetto::{trace_packet::Data, Trace, TrackDescriptor, TrackEvent};
use prost::Message;
use rstest::{fixture, rstest};
use serial_test::serial;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct TestSetup {
    _temp_dir: TempDir,
    config_path: PathBuf,
    socket_path: PathBuf,
    output_path: PathBuf,
}

impl TestSetup {
    fn new() -> Result<Self> {
        let temp_dir = TempDir::new()?;
        let config_path = temp_dir.path().join("config.toml");
        let socket_path = temp_dir.path().join("test.sock");
        let output_path = temp_dir.path().join("trace.perfetto");

        let config_content = format!(
            r#"
[global]
buffer_size = 1048576

[[user]]
socket = "{}"
log_filter = "debug"
"#,
            socket_path.display()
        );

        fs::write(&config_path, config_content)?;

        Ok(TestSetup {
            _temp_dir: temp_dir,
            config_path,
            socket_path,
            output_path,
        })
    }
}

#[fixture]
fn setup() -> TestSetup {
    TestSetup::new().expect("failed to create test setup")
}

struct ManagedProcess {
    child: Option<Child>,
}

impl ManagedProcess {
    fn new(name: &str, mut command: Command) -> Result<Self> {
        let child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn {}", name))?;

        Ok(ManagedProcess { child: Some(child) })
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            match child.try_wait()? {
                Some(_) => {}
                None => {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(child.id() as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );

                    let timeout = Duration::from_secs(5);
                    let start = Instant::now();

                    loop {
                        match child.try_wait()? {
                            Some(_) => break,
                            None => {
                                if start.elapsed() > timeout {
                                    child.kill()?;
                                    child.wait()?;
                                    break;
                                }
                                thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }
                }
            }

            Ok(())
        } else {
            Ok(())
        }
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn find_binary(name: &str) -> Result<PathBuf> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace_root = Path::new(manifest_dir).parent().unwrap();

    let debug_path = workspace_root.join("target/debug").join(name);
    if debug_path.exists() {
        return Ok(debug_path);
    }

    let examples_path = workspace_root.join("target/debug/examples").join(name);
    if examples_path.exists() {
        return Ok(examples_path);
    }

    eyre::bail!(
        "binary '{}' not found in target/debug or target/debug/examples",
        name
    )
}

fn wait_for_socket(socket_path: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while !socket_path.exists() {
        if start.elapsed() > timeout {
            eyre::bail!("timeout waiting for socket at {:?}", socket_path);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn parse_trace_file(path: &Path) -> Result<Trace> {
    let mut file = fs::File::open(path)?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;

    let trace = Trace::decode(&contents[..])?;
    Ok(trace)
}

fn find_track_descriptors(trace: &Trace) -> Vec<&TrackDescriptor> {
    trace
        .packet
        .iter()
        .filter_map(|packet| match &packet.data {
            Some(Data::TrackDescriptor(desc)) => Some(desc),
            _ => None,
        })
        .collect()
}

fn find_track_events(trace: &Trace) -> Vec<&TrackEvent> {
    trace
        .packet
        .iter()
        .filter_map(|packet| match &packet.data {
            Some(Data::TrackEvent(event)) => Some(event),
            _ => None,
        })
        .collect()
}

fn has_counter_events(trace: &Trace) -> bool {
    trace.packet.iter().any(|packet| {
        if let Some(Data::TrackEvent(event)) = &packet.data {
            event.r#type() == perfetto_format::perfetto::track_event::Type::Counter
        } else {
            false
        }
    })
}

fn has_interned_data(trace: &Trace) -> bool {
    trace
        .packet
        .iter()
        .any(|packet| packet.interned_data.is_some())
}

fn has_perf_samples(trace: &Trace) -> bool {
    trace
        .packet
        .iter()
        .any(|packet| matches!(&packet.data, Some(Data::PerfSample(_))))
}

#[rstest]
#[serial]
fn test_basic_integration(setup: TestSetup) -> Result<()> {
    let manytrace_bin = find_binary("manytrace")?;
    let continuous_bin = find_binary("continuous")?;

    let mut continuous_cmd = Command::new(&continuous_bin);
    continuous_cmd.arg(&setup.socket_path);

    let mut continuous = ManagedProcess::new("continuous", continuous_cmd)?;

    wait_for_socket(&setup.socket_path, Duration::from_secs(10))?;

    let mut manytrace_cmd = Command::new(&manytrace_bin);
    manytrace_cmd
        .arg(&setup.config_path)
        .arg("-o")
        .arg(&setup.output_path)
        .arg("-d")
        .arg("5s");

    let mut manytrace = ManagedProcess::new("manytrace", manytrace_cmd)?;

    thread::sleep(Duration::from_secs(2));

    continuous.stop()?;
    manytrace.stop()?;

    assert!(setup.output_path.exists(), "trace output file should exist");

    let file_size = fs::metadata(&setup.output_path)?.len();
    assert!(file_size > 0, "trace file should not be empty");

    let trace = parse_trace_file(&setup.output_path)?;
    assert!(!trace.packet.is_empty(), "trace should contain packets");

    Ok(())
}

#[rstest]
#[serial]
fn test_event_verification(setup: TestSetup) -> Result<()> {
    let manytrace_bin = find_binary("manytrace")?;
    let continuous_bin = find_binary("continuous")?;

    let mut continuous_cmd = Command::new(&continuous_bin);
    continuous_cmd.arg(&setup.socket_path);

    let mut continuous = ManagedProcess::new("continuous", continuous_cmd)?;

    wait_for_socket(&setup.socket_path, Duration::from_secs(10))?;

    let mut manytrace_cmd = Command::new(&manytrace_bin);
    manytrace_cmd
        .arg(&setup.config_path)
        .arg("-o")
        .arg(&setup.output_path)
        .arg("-d")
        .arg("5s");

    let mut manytrace = ManagedProcess::new("manytrace", manytrace_cmd)?;

    thread::sleep(Duration::from_secs(3));

    continuous.stop()?;
    manytrace.stop()?;

    let trace = parse_trace_file(&setup.output_path)?;

    let track_descriptors = find_track_descriptors(&trace);
    assert!(
        !track_descriptors.is_empty(),
        "should have track descriptors"
    );

    let has_thread_descriptor = track_descriptors.iter().any(|desc| desc.thread.is_some());
    assert!(has_thread_descriptor, "should have thread descriptors");

    let has_process_descriptor = track_descriptors.iter().any(|desc| desc.process.is_some());
    assert!(has_process_descriptor, "should have process descriptors");

    let track_events = find_track_events(&trace);
    assert!(!track_events.is_empty(), "should have track events");

    let thread_names: Vec<String> = track_descriptors
        .iter()
        .filter_map(|desc| desc.thread.as_ref().and_then(|t| t.thread_name.clone()))
        .collect();

    let expected_threads = ["worker-0", "worker-1", "worker-2", "worker-3", "monitor"];
    for expected in &expected_threads {
        assert!(
            thread_names.iter().any(|name| name.contains(expected)),
            "should find thread {}",
            expected
        );
    }

    Ok(())
}

#[rstest]
#[serial]
fn test_multiple_clients(setup: TestSetup) -> Result<()> {
    let manytrace_bin = find_binary("manytrace")?;
    let continuous_bin = find_binary("continuous")?;

    let config_content = format!(
        r#"
[global]
buffer_size = 2097152

[[user]]
socket = "{}/client1.sock"
log_filter = "info"

[[user]]
socket = "{}/client2.sock"
log_filter = "debug"
"#,
        setup._temp_dir.path().display(),
        setup._temp_dir.path().display()
    );

    fs::write(&setup.config_path, config_content)?;

    let mut manytrace_cmd = Command::new(&manytrace_bin);
    manytrace_cmd
        .arg(&setup.config_path)
        .arg("-o")
        .arg(&setup.output_path)
        .arg("-d")
        .arg("5s");

    let socket1 = setup._temp_dir.path().join("client1.sock");
    let socket2 = setup._temp_dir.path().join("client2.sock");

    let mut continuous1_cmd = Command::new(&continuous_bin);
    continuous1_cmd.arg(&socket1);
    let mut continuous1 = ManagedProcess::new("continuous1", continuous1_cmd)?;

    let mut continuous2_cmd = Command::new(&continuous_bin);
    continuous2_cmd.arg(&socket2);
    let mut continuous2 = ManagedProcess::new("continuous2", continuous2_cmd)?;

    wait_for_socket(&socket1, Duration::from_secs(10))?;
    wait_for_socket(&socket2, Duration::from_secs(10))?;

    let mut manytrace = ManagedProcess::new("manytrace", manytrace_cmd)?;

    thread::sleep(Duration::from_secs(3));

    continuous1.stop()?;
    continuous2.stop()?;
    manytrace.stop()?;

    let trace = parse_trace_file(&setup.output_path)?;

    let track_descriptors = find_track_descriptors(&trace);
    let process_count = track_descriptors
        .iter()
        .filter(|desc| desc.process.is_some())
        .count();

    assert!(
        process_count >= 2,
        "should have at least 2 process descriptors"
    );

    Ok(())
}

fn check_sudo_available() -> bool {
    Command::new("sudo")
        .arg("-n")
        .arg("true")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[rstest]
#[serial]
#[ignore = "requires sudo privileges - run with 'cargo test -- --ignored' or 'cargo test test_sudo_profiler -- --include-ignored'"]
fn test_sudo_profiler() -> Result<()> {
    if !check_sudo_available() {
        return Ok(());
    }

    let temp_dir = TempDir::new()?;
    let config_path = temp_dir.path().join("config.toml");
    let socket_path = temp_dir.path().join("test.sock");
    let output_path = temp_dir.path().join("trace.perfetto");

    let config_content = format!(
        r#"
[global]
buffer_size = 1048576

[bpf.thread_tracker]

[bpf.profiler]
sample_freq = 999
kernel_samples = true
user_samples = true
filter_process = ["continuous"]

[[user]]
socket = "{}"
log_filter = "debug"
"#,
        socket_path.display()
    );

    fs::write(&config_path, config_content)?;

    let manytrace_bin = find_binary("manytrace")?;
    let continuous_bin = find_binary("continuous")?;

    let mut continuous_cmd = Command::new(&continuous_bin);
    continuous_cmd.arg(&socket_path);
    let mut continuous = ManagedProcess::new("continuous", continuous_cmd)?;

    wait_for_socket(&socket_path, Duration::from_secs(10))?;

    let mut manytrace_cmd = Command::new("sudo");
    manytrace_cmd
        .arg(&manytrace_bin)
        .arg(&config_path)
        .arg("-o")
        .arg(&output_path)
        .arg("-d")
        .arg("5s");

    let mut manytrace = ManagedProcess::new("manytrace-sudo", manytrace_cmd)?;

    thread::sleep(Duration::from_secs(3));

    continuous.stop()?;
    manytrace.stop()?;

    let trace = parse_trace_file(&output_path)?;

    assert!(!trace.packet.is_empty(), "trace should have packets");

    let track_descriptors = find_track_descriptors(&trace);
    assert!(
        !track_descriptors.is_empty(),
        "should have track descriptors"
    );

    let track_events = find_track_events(&trace);
    assert!(
        !track_events.is_empty(),
        "should have track events from profiler"
    );

    assert!(
        has_interned_data(&trace),
        "should have interned data for symbols and stack frames"
    );

    assert!(
        has_perf_samples(&trace),
        "should have perf samples from profiler"
    );

    Ok(())
}

#[rstest]
#[serial]
#[ignore = "requires sudo privileges - run with 'cargo test -- --ignored' or 'cargo test test_sudo_cpuutil -- --include-ignored'"]
fn test_sudo_cpuutil() -> Result<()> {
    if !check_sudo_available() {
        return Ok(());
    }

    let temp_dir = TempDir::new()?;
    let config_path = temp_dir.path().join("config.toml");
    let socket_path = temp_dir.path().join("test.sock");
    let output_path = temp_dir.path().join("trace.perfetto");

    let config_content = format!(
        r#"
[global]
buffer_size = 1048576

[bpf.thread_tracker]

[bpf.cpu_util]
frequency = 10
filter_process = ["continuous"]

[[user]]
socket = "{}"
log_filter = "debug"
"#,
        socket_path.display()
    );

    fs::write(&config_path, config_content)?;

    let manytrace_bin = find_binary("manytrace")?;
    let continuous_bin = find_binary("continuous")?;

    let mut continuous_cmd = Command::new(&continuous_bin);
    continuous_cmd.arg(&socket_path);
    let mut continuous = ManagedProcess::new("continuous", continuous_cmd)?;

    wait_for_socket(&socket_path, Duration::from_secs(10))?;

    let mut manytrace_cmd = Command::new("sudo");
    manytrace_cmd
        .arg(&manytrace_bin)
        .arg(&config_path)
        .arg("-o")
        .arg(&output_path)
        .arg("-d")
        .arg("5s");

    let mut manytrace = ManagedProcess::new("manytrace-sudo", manytrace_cmd)?;

    thread::sleep(Duration::from_secs(3));

    continuous.stop()?;
    manytrace.stop()?;

    let trace = parse_trace_file(&output_path)?;

    assert!(!trace.packet.is_empty(), "trace should have packets");

    let track_descriptors = find_track_descriptors(&trace);
    assert!(
        !track_descriptors.is_empty(),
        "should have track descriptors"
    );

    let has_process = track_descriptors.iter().any(|desc| desc.process.is_some());
    let has_thread = track_descriptors.iter().any(|desc| desc.thread.is_some());
    assert!(has_process, "should have process descriptors from CPU util");
    assert!(has_thread, "should have thread descriptors from CPU util");

    assert!(
        has_counter_events(&trace),
        "should have counter events for CPU utilization"
    );

    Ok(())
}
