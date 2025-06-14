use divan::Bencher;
use protocol::LogLevel;
use std::sync::Arc;
use tempfile::TempDir;
use tracing::info;
use tracing_manytrace::ManytraceLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

fn main() {
    divan::main();
}

struct BenchSetup {
    _temp_dir: TempDir,
    _client: agent::AgentClient,
    agent: Arc<agent::Agent>,
}

fn setup_tracing() -> BenchSetup {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("bench.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let agent = Arc::new(agent::Agent::new(socket_path_str.clone()).unwrap());

    let consumer = agent::Consumer::new(128 << 20).unwrap();
    let mut client = agent::AgentClient::new(socket_path_str);

    for i in 0..10 {
        match client.start(&consumer, LogLevel::Debug) {
            Ok(_) => break,
            Err(_) => {
                if i == 9 {
                    panic!("Failed to connect client after 10 attempts");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    for i in 0..10 {
        if agent.enabled() {
            break;
        }
        if i == 9 {
            panic!("Agent never became enabled");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    BenchSetup {
        _temp_dir: temp_dir,
        _client: client,
        agent,
    }
}

#[divan::bench]
fn bench_info_event(bencher: Bencher) {
    let setup = setup_tracing();
    let layer = ManytraceLayer::new(setup.agent);
    let subscriber = Registry::default().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);
    bencher.bench_local(|| {
        info!("test message");
    });
}

#[divan::bench]
fn bench_info_event_with_fields(bencher: Bencher) {
    let setup = setup_tracing();
    let layer = ManytraceLayer::new(setup.agent);
    let subscriber = Registry::default().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);
    bencher.bench_local(|| {
        info!(user_id = 123, request_id = "abc123", "processing request");
    });
}
