use agent::{Agent, AgentBuilder, AgentClient, Consumer};
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
    extension: Arc<crate::TracingExtension>,
    _temp_dir: TempDir,
    _agent: Agent,
}

#[fixture]
fn setup() -> TestSetup {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let socket_path = temp_dir.path().join("test.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();

    let extension = Arc::new(crate::TracingExtension::new());
    let agent = AgentBuilder::new(socket_path_str.clone())
        .register_tracing(Box::new((*extension).clone()))
        .build()
        .expect("failed to create agent");

    let consumer = Consumer::new(1024 * 1024).expect("failed to create consumer");
    let mut client = AgentClient::new(socket_path_str);

    for i in 0..10 {
        match client.start(&consumer, "debug".to_string()) {
            Ok(_) => break,
            Err(_) => {
                if i == 9 {
                    panic!("Failed to connect client after 10 attempts");
                }
                sleep(Duration::from_millis(50));
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
        sleep(Duration::from_millis(50));
    }

    TestSetup {
        consumer,
        client,
        extension,
        _temp_dir: temp_dir,
        _agent: agent,
    }
}

#[rstest]
fn test_basic_span(mut setup: TestSetup) {
    let layer = crate::ManytraceLayer::new(setup.extension);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = info_span!("test_span");
        let _guard = span.enter();
    });

    setup
        .client
        .send_continue()
        .expect("failed to send keepalive");

    let mut span_count = 0;
    let mut _other_count = 0;
    while let Some(record) = setup.consumer.consume() {
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
    let layer = crate::ManytraceLayer::new(setup.extension);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("my_span", answer = 42, name = "test");
        let _guard = span.enter();
    });
    let mut found_span_with_fields = false;
    let mut found_process_name = false;
    let mut found_thread_name = false;

    while let Some(record) = setup.consumer.consume() {
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
            Ok(protocol::ArchivedEvent::Track(track)) => match &track.track_type {
                protocol::ArchivedTrackType::Process { .. } => {
                    found_process_name = true;
                }
                protocol::ArchivedTrackType::Thread { .. } => {
                    found_thread_name = true;
                }
                _ => {}
            },
            _ => {}
        }
    }
    assert!(found_span_with_fields, "should find span with fields");
    assert!(found_process_name, "should emit process track");
    assert!(found_thread_name, "should emit thread track");
    setup.client.stop().expect("failed to stop client");
}

#[rstest]
fn test_thread_process_names(mut setup: TestSetup) {
    let layer = crate::ManytraceLayer::new(setup.extension);
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

    while let Some(record) = setup.consumer.consume() {
        match record.as_event() {
            Ok(protocol::ArchivedEvent::Track(track)) => match &track.track_type {
                protocol::ArchivedTrackType::Process { pid } => {
                    found_process_name = true;
                    assert!(pid.to_native() > 0);
                }
                protocol::ArchivedTrackType::Thread { tid, pid } => {
                    if track.name.as_bytes() == b"test-worker" {
                        found_thread_name = true;
                        assert!(tid.to_native() > 0);
                        assert!(pid.to_native() > 0);
                    }
                }
                _ => {}
            },
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

    assert!(found_process_name, "should emit process track");
    assert!(found_thread_name, "should emit thread track");
    assert!(found_event, "should emit the test event");

    setup.client.stop().expect("failed to stop client");
}
