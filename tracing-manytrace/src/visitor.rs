use crate::layer::StoredLabels;
use std::fmt;
use tracing::field::{Field, Visit};

impl Visit for StoredLabels {
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
        self.strings_storage.insert(field.name(), value.to_string());
        let stored_value = self.strings_storage.get(field.name()).unwrap();
        let value_ref = unsafe { std::mem::transmute::<&str, &'static str>(stored_value.as_str()) };
        self.labels.strings.insert(field.name(), value_ref);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let formatted = format!("{:?}", value);
        self.strings_storage.insert(field.name(), formatted);
        let stored_value = self.strings_storage.get(field.name()).unwrap();
        let value_ref = unsafe { std::mem::transmute::<&str, &'static str>(stored_value.as_str()) };
        self.labels.strings.insert(field.name(), value_ref);
    }
}
