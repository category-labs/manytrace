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

use protocol::{ArchivedLabels, Labels};
use rkyv::boxed::ArchivedBox;
use rkyv::collections::swiss_table::map::Iter as ArchivedMapIter;
use rkyv::hash::FxHasher64;
use rkyv::string::ArchivedString;
use rkyv::Archived;
use std::borrow::Cow;

pub trait LabelIterator {
    type StringIter<'a>: Iterator<Item = (&'a str, Cow<'a, str>)>
    where
        Self: 'a;
    type IntIter<'a>: Iterator<Item = (&'a str, i64)>
    where
        Self: 'a;
    type BoolIter<'a>: Iterator<Item = (&'a str, bool)>
    where
        Self: 'a;
    type FloatIter<'a>: Iterator<Item = (&'a str, f64)>
    where
        Self: 'a;

    fn iter_strings(&self) -> Self::StringIter<'_>;
    fn iter_ints(&self) -> Self::IntIter<'_>;
    fn iter_bools(&self) -> Self::BoolIter<'_>;
    fn iter_floats(&self) -> Self::FloatIter<'_>;
}

impl LabelIterator for Labels<'_> {
    type StringIter<'a>
        = std::iter::Map<
        std::collections::hash_map::Iter<'a, &'a str, Cow<'a, str>>,
        fn((&'a &'a str, &'a Cow<'a, str>)) -> (&'a str, Cow<'a, str>),
    >
    where
        Self: 'a;

    type IntIter<'a>
        = std::iter::Map<
        std::collections::hash_map::Iter<'a, &'a str, i64>,
        fn((&'a &'a str, &'a i64)) -> (&'a str, i64),
    >
    where
        Self: 'a;

    type BoolIter<'a>
        = std::iter::Map<
        std::collections::hash_map::Iter<'a, &'a str, bool>,
        fn((&'a &'a str, &'a bool)) -> (&'a str, bool),
    >
    where
        Self: 'a;

    type FloatIter<'a>
        = std::iter::Map<
        std::collections::hash_map::Iter<'a, &'a str, f64>,
        fn((&'a &'a str, &'a f64)) -> (&'a str, f64),
    >
    where
        Self: 'a;

    fn iter_strings(&self) -> Self::StringIter<'_> {
        self.strings.iter().map(|(k, v)| (*k, v.clone()))
    }

    fn iter_ints(&self) -> Self::IntIter<'_> {
        self.ints.iter().map(|(k, v)| (*k, *v))
    }

    fn iter_bools(&self) -> Self::BoolIter<'_> {
        self.bools.iter().map(|(k, v)| (*k, *v))
    }

    fn iter_floats(&self) -> Self::FloatIter<'_> {
        self.floats.iter().map(|(k, v)| (*k, *v))
    }
}

type ArchivedF64 = Archived<f64>;

impl LabelIterator for ArchivedLabels<'_> {
    type StringIter<'a>
        = std::iter::Map<
        ArchivedMapIter<'a, ArchivedBox<str>, ArchivedString, FxHasher64>,
        fn((&'a ArchivedBox<str>, &'a ArchivedString)) -> (&'a str, Cow<'a, str>),
    >
    where
        Self: 'a;

    type IntIter<'a>
        = std::iter::Map<
        ArchivedMapIter<'a, ArchivedBox<str>, Archived<i64>, FxHasher64>,
        fn((&'a ArchivedBox<str>, &'a Archived<i64>)) -> (&'a str, i64),
    >
    where
        Self: 'a;

    type BoolIter<'a>
        = std::iter::Map<
        ArchivedMapIter<'a, ArchivedBox<str>, Archived<bool>, FxHasher64>,
        fn((&'a ArchivedBox<str>, &'a Archived<bool>)) -> (&'a str, bool),
    >
    where
        Self: 'a;

    type FloatIter<'a>
        = std::iter::Map<
        ArchivedMapIter<'a, ArchivedBox<str>, ArchivedF64, FxHasher64>,
        fn((&'a ArchivedBox<str>, &'a ArchivedF64)) -> (&'a str, f64),
    >
    where
        Self: 'a;

    fn iter_strings(&self) -> Self::StringIter<'_> {
        self.strings
            .iter()
            .map(|(k, v)| (k.as_ref(), Cow::Borrowed(v.as_ref())))
    }

    fn iter_ints(&self) -> Self::IntIter<'_> {
        self.ints.iter().map(|(k, v)| (k.as_ref(), v.to_native()))
    }

    fn iter_bools(&self) -> Self::BoolIter<'_> {
        self.bools.iter().map(|(k, v)| (k.as_ref(), *v))
    }

    fn iter_floats(&self) -> Self::FloatIter<'_> {
        self.floats.iter().map(|(k, v)| (k.as_ref(), v.to_native()))
    }
}
