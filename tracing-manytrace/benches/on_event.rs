use divan::Bencher;
use std::sync::Arc;
use tempfile::TempDir;
use tracing::info;
use tracing_manytrace::{ManytraceLayer, TracingExtension};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

fn main() {
    divan::main();
}

struct BenchSetup {
    _temp_dir: TempDir,
    _client: agent::AgentClient,
    _agent: agent::Agent,
    extension: Arc<TracingExtension>,
}

fn setup_tracing() -> BenchSetup {
    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("bench.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let extension = Arc::new(TracingExtension::new());
    let agent = agent::AgentBuilder::new(socket_path_str.clone())
        .register_tracing(Box::new((*extension).clone()))
        .build()
        .unwrap();

    let consumer = agent::Consumer::new(128 << 20).unwrap();
    let mut client = agent::AgentClient::new(socket_path_str);

    for i in 0..10 {
        match client.start(&consumer, "debug".to_string()) {
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
        if extension.is_active() {
            break;
        }
        if i == 9 {
            panic!("Extension never became active");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    BenchSetup {
        _temp_dir: temp_dir,
        _client: client,
        _agent: agent,
        extension,
    }
}

#[divan::bench]
fn bench_info_event(bencher: Bencher) {
    let setup = setup_tracing();
    let layer = ManytraceLayer::new(setup.extension);
    let subscriber = Registry::default().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);
    bencher.bench_local(|| {
        info!("test message");
    });
}

#[divan::bench]
fn bench_info_event_with_fields(bencher: Bencher) {
    let setup = setup_tracing();
    let layer = ManytraceLayer::new(setup.extension);
    let subscriber = Registry::default().with(layer);
    let _guard = tracing::subscriber::set_default(subscriber);
    bencher.bench_local(|| {
        info!(user_id = 123, request_id = "abc123", "processing request");
    });
}
