use mpscbuf::{Consumer as MpscConsumer, Producer as MpscProducer};
use protocol::{ArchivedEvent, Event};

use crate::Result;

/// Producer end of the shared memory channel.
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

/// Consumer end of the shared memory channel.
pub struct Consumer {
    inner: MpscConsumer,
}

impl Consumer {
    /// Create a new consumer with the given buffer size.
    pub fn new(buffer_size: usize) -> Result<Self> {
        let inner = MpscConsumer::new(buffer_size)?;
        Ok(Consumer { inner })
    }

    /// Get the next available event.
    pub fn consume(&mut self) -> Option<Record> {
        self.inner.consume().map(Record)
    }

    /// Block until new events are available.
    pub fn wait(&mut self) -> Result<()> {
        Ok(self.inner.wait()?)
    }

    /// Number of events available to read.
    pub fn available_records(&self) -> u64 {
        self.inner.available_records()
    }

    /// Number of events dropped due to buffer overflow.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped()
    }

    pub(crate) fn memory_fd(&self) -> std::os::fd::BorrowedFd {
        self.inner.memory_fd()
    }

    pub(crate) fn notification_fd(&self) -> std::os::fd::BorrowedFd {
        self.inner.notification_fd()
    }

    pub(crate) fn data_size(&self) -> usize {
        self.inner.data_size()
    }
}

/// A single event record from the buffer.
pub struct Record<'a>(mpscbuf::Record<'a>);

impl<'a> Record<'a> {
    /// Access the event data.
    pub fn as_event(&self) -> std::result::Result<&ArchivedEvent, rkyv::rancor::Error> {
        rkyv::access::<ArchivedEvent, rkyv::rancor::Error>(self.0.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpscbuf::WakeupStrategy;
    use protocol::{Counter, Labels};
    use std::borrow::Cow;

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
            labels: Cow::Owned(Labels::new()),
            unit: None
        });

        producer.submit(&counter).unwrap();

        let mut events_received = 0;
        while let Some(record) = consumer.consume() {
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
