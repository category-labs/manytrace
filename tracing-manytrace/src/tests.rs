use agent::{Agent, AgentClient, Consumer};
use protocol::LogLevel;
use rstest::{fixture, rstest};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tempfile::TempDir;
use tracing::info_span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

struct TestSetup {
    consumer: Consumer,
    client: AgentClient,
    agent: Arc<Agent>,
    _temp_dir: TempDir,
}

#[fixture]
fn setup() -> TestSetup {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let socket_path = temp_dir.path().join("test.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let agent = Agent::new(socket_path_str.clone()).expect("failed to create agent");
    let agent_arc = Arc::new(agent);

    let consumer = Consumer::new(1024 * 1024).expect("failed to create consumer");
    let mut client = AgentClient::new(socket_path_str);

    for i in 0..10 {
        match client.start(&consumer, LogLevel::Debug) {
            Ok(_) => break,
            Err(_) => {
                if i == 9 {
                    panic!("Failed to connect client after 10 attempts");
                }
                sleep(Duration::from_millis(50));
            }
        }
    }

    // Wait for agent to be enabled
    for i in 0..10 {
        if agent_arc.enabled() {
            break;
        }
        if i == 9 {
            panic!("Agent never became enabled");
        }
        sleep(Duration::from_millis(50));
    }

    TestSetup {
        consumer,
        client,
        agent: agent_arc,
        _temp_dir: temp_dir,
    }
}

#[rstest]
fn test_basic_span(mut setup: TestSetup) {
    let layer = crate::ManytraceLayer::new(setup.agent);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = info_span!("test_span");
        let _guard = span.enter();
        // Span should be emitted when dropped
    });

    setup
        .client
        .send_continue()
        .expect("failed to send keepalive");

    let mut span_count = 0;
    let mut _other_count = 0;
    for record in setup.consumer.iter() {
        match record.as_event() {
            Ok(protocol::ArchivedEvent::Span(span)) => {
                span_count += 1;
                assert_eq!(span.name.as_bytes(), b"test_span");
                assert!(span.start_timestamp.to_native() > 0);
                assert!(span.end_timestamp.to_native() > span.start_timestamp.to_native());
            }
            Ok(_) => _other_count += 1,
            Err(_) => {}
        }
    }

    assert!(span_count > 0, "Expected at least one span event");
    setup.client.stop().expect("failed to stop client");
}

#[rstest]
fn test_span_with_fields(mut setup: TestSetup) {
    let layer = crate::ManytraceLayer::new(setup.agent);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("my_span", answer = 42, name = "test");
        let _guard = span.enter();
    });
    let mut found_span_with_fields = false;
    let mut found_process_name = false;
    let mut found_thread_name = false;

    for record in setup.consumer.iter() {
        match record.as_event() {
            Ok(protocol::ArchivedEvent::Span(span)) => {
                if span.name.as_bytes() == b"my_span" {
                    assert!(span.labels.ints.get("answer").is_some());
                    assert!(span.labels.strings.get("name").is_some());
                    assert!(span.start_timestamp.to_native() > 0);
                    assert!(span.end_timestamp.to_native() > span.start_timestamp.to_native());
                    found_span_with_fields = true;
                }
            }
            Ok(protocol::ArchivedEvent::ProcessName(_)) => {
                found_process_name = true;
            }
            Ok(protocol::ArchivedEvent::ThreadName(_)) => {
                found_thread_name = true;
            }
            _ => {}
        }
    }
    assert!(found_span_with_fields, "should find span with fields");
    assert!(found_process_name, "should emit process name");
    assert!(found_thread_name, "should emit thread name");
    setup.client.stop().expect("failed to stop client");
}

#[rstest]
fn test_thread_process_names(mut setup: TestSetup) {
    let layer = crate::ManytraceLayer::new(setup.agent);
    let subscriber = Registry::default().with(layer);

    let handle = std::thread::Builder::new()
        .name("test-worker".to_string())
        .spawn(move || {
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!("test event from worker thread");
            });
        })
        .expect("failed to spawn thread");

    handle.join().expect("thread panicked");

    setup
        .client
        .send_continue()
        .expect("failed to send keepalive");

    let mut found_process_name = false;
    let mut found_thread_name = false;
    let mut found_event = false;

    for record in setup.consumer.iter() {
        match record.as_event() {
            Ok(protocol::ArchivedEvent::ProcessName(process)) => {
                found_process_name = true;
                assert!(process.pid.to_native() > 0);
            }
            Ok(protocol::ArchivedEvent::ThreadName(thread)) => {
                if thread.name.as_bytes() == b"test-worker" {
                    found_thread_name = true;
                    assert!(thread.tid.to_native() > 0);
                    assert!(thread.pid.to_native() > 0);
                }
            }
            Ok(protocol::ArchivedEvent::Instant(instant)) => {
                let name = std::str::from_utf8(instant.name.as_bytes()).unwrap_or("");
                if name.contains("tracing-manytrace/src/tests.rs") {
                    found_event = true;
                }
            }
            Ok(_) => {}
            Err(_) => {}
        }
    }

    assert!(found_process_name, "should emit process name");
    assert!(found_thread_name, "should emit thread name");
    assert!(found_event, "should emit the test event");

    setup.client.stop().expect("failed to stop client");
}
