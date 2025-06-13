use mpscbuf::Producer as MpscProducer;
use protocol::Event;

use crate::Result;

pub struct Producer {
    inner: MpscProducer,
}

impl Producer {
    pub(crate) fn from_inner(inner: MpscProducer) -> Self {
        Producer { inner }
    }

    pub(crate) fn submit(&self, event: &Event) -> Result<()> {
        let required_size = protocol::compute_length(event)?;

        let mut reserved = self.inner.reserve(required_size)?;

        protocol::serialize_to_buf(event, reserved.as_mut_slice())?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Consumer;
    use mpscbuf::WakeupStrategy;
    use protocol::{Counter, Labels};

    #[test]
    fn test_producer_consumer_single_thread() {
        let buffer_size = 1024 * 1024;
        let mut consumer = Consumer::new(buffer_size).unwrap();

        let producer = MpscProducer::new(
            consumer.memory_fd().try_clone_to_owned().unwrap(),
            consumer.notification_fd().try_clone_to_owned().unwrap(),
            consumer.data_size(),
            WakeupStrategy::NoWakeup,
        )
        .unwrap();

        let producer = Producer::from_inner(producer);

        let counter = protocol::Event::Counter(Counter {
            name: "test_counter",
            value: 42.0,
            timestamp: 1000,
            tid: 123,
            pid: 456,
            labels: Labels::new(),
        });

        producer.submit(&counter).unwrap();

        let mut events_received = 0;
        for record in consumer.iter() {
            if let Ok(archived_event) = record.as_event() {
                match archived_event {
                    protocol::ArchivedEvent::Counter(archived_counter) => {
                        assert_eq!(archived_counter.name.as_bytes(), b"test_counter");
                        assert_eq!(archived_counter.value.to_native(), 42.0);
                        assert_eq!(archived_counter.timestamp.to_native(), 1000);
                        assert_eq!(archived_counter.tid.to_native(), 123);
                        assert_eq!(archived_counter.pid.to_native(), 456);
                        events_received += 1;
                        break;
                    }
                    _ => panic!("Expected Counter event"),
                }
            }
        }

        assert_eq!(events_received, 1);
    }
}
