use mpscbuf::{Consumer as MpscConsumer, ConsumerIter};
use protocol::ArchivedEvent;

use crate::Result;

pub struct Consumer {
    inner: MpscConsumer,
}

impl Consumer {
    pub fn new(buffer_size: usize) -> Result<Self> {
        let inner = MpscConsumer::new(buffer_size)?;
        Ok(Consumer { inner })
    }

    pub fn iter(&mut self) -> EventIter {
        EventIter::new(&mut self.inner)
    }

    pub fn wait(&mut self) -> Result<()> {
        Ok(self.inner.wait()?)
    }

    pub fn available_records(&self) -> u64 {
        self.inner.available_records()
    }

    pub fn dropped(&self) -> u64 {
        self.inner.dropped()
    }
}

pub struct EventIter<'a> {
    iterator: ConsumerIter<'a>,
}

impl<'a> EventIter<'a> {
    fn new(consumer: &'a mut MpscConsumer) -> Self {
        EventIter { iterator: consumer.iter() }
    }
}

pub struct Record<'a>(mpscbuf::Record<'a>);

impl<'a> Record<'a> {
    pub fn as_event(&self) -> std::result::Result<&ArchivedEvent, rkyv::rancor::Error> {
        rkyv::access::<ArchivedEvent, rkyv::rancor::Error>(self.0.as_slice())
    }
}

impl<'a> Iterator for EventIter<'a> {
    type Item = Record<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iterator.next().map(Record)
    }
}
