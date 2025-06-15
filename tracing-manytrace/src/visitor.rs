use crate::layer::SpanData;
use std::borrow::Cow;
use std::fmt;
use tracing::field::{Field, Visit};

impl Visit for SpanData {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.labels.ints.insert(field.name(), value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if value <= i64::MAX as u64 {
            self.labels.ints.insert(field.name(), value as i64);
        } else {
            self.labels.floats.insert(field.name(), value as f64);
        }
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.labels.floats.insert(field.name(), value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.labels.bools.insert(field.name(), value);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.labels
            .strings
            .insert(field.name(), Cow::Owned(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let formatted = format!("{:?}", value);
        self.labels
            .strings
            .insert(field.name(), Cow::Owned(formatted));
    }
}
