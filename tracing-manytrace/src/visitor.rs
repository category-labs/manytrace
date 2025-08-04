// Copyright (C) 2025 Category Labs, Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

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
