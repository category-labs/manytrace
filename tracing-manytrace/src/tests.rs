#[cfg(test)]
use agent::{Agent, AgentClient, Consumer};
#[cfg(test)]
use protocol::LogLevel;
#[cfg(test)]
use rstest::{fixture, rstest};
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tempfile::TempDir;
#[cfg(test)]
use tracing::info_span;
#[cfg(test)]
use tracing_subscriber::layer::SubscriberExt;
#[cfg(test)]
use tracing_subscriber::Registry;

#[cfg(test)]
struct TestSetup {
    consumer: Consumer,
    client: AgentClient,
    agent: Arc<Agent>,
    _temp_dir: TempDir,
}

#[cfg(test)]
#[fixture]
fn setup() -> TestSetup {
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let socket_path = temp_dir.path().join("test.sock");
    let socket_path_str = socket_path.to_string_lossy().to_string();
    let agent = Agent::new(socket_path_str.clone()).expect("failed to create agent");
    let agent_arc = Arc::new(agent);
    let consumer = Consumer::new(1024 * 1024).expect("failed to create consumer");
    let mut client = AgentClient::new(socket_path_str);
    for _ in 0..10 {
        if client.start(&consumer, LogLevel::Debug).is_ok() {
            break;
        }
        std::thread::yield_now();
    }
    TestSetup {
        consumer,
        client,
        agent: agent_arc,
        _temp_dir: temp_dir,
    }
}

#[cfg(test)]
#[rstest]
fn test_basic_span(mut setup: TestSetup) {
    let layer = crate::ManytraceLayer::new(setup.agent);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = info_span!("test_span");
        let _guard = span.enter();
    });
    setup
        .client
        .send_continue()
        .expect("failed to send keepalive");
    let mut count = 0;
    for record in setup.consumer.iter() {
        if record.as_event().is_ok() {
            count += 1;
        }
    }
    assert!(count > 0);
    setup.client.stop().expect("failed to stop client");
}

#[cfg(test)]
#[rstest]
fn test_span_with_fields(mut setup: TestSetup) {
    let layer = crate::ManytraceLayer::new(setup.agent);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("my_span", answer = 42, name = "test");
        let _guard = span.enter();
    });
    setup
        .client
        .send_continue()
        .expect("failed to send keepalive");
    let mut found_span_with_fields = false;
    for record in setup.consumer.iter() {
        if let Ok(protocol::ArchivedEvent::Span(span)) = record.as_event() {
            if span.name.as_bytes() == b"my_span" {
                assert!(span.labels.ints.get("answer").is_some());
                assert!(span.labels.strings.get("name").is_some());
                found_span_with_fields = true;
            }
        }
    }
    assert!(found_span_with_fields, "should find span with fields");
    setup.client.stop().expect("failed to stop client");
}