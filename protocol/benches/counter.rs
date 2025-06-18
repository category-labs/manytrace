use divan::Bencher;
use protocol::{compute_length, serialize_to_buf, Counter, Labels};
use rkyv::api::high::to_bytes_in;
use rkyv::util::AlignedVec;
use std::borrow::Cow;
use std::collections::HashMap;
use std::hint::black_box;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn create_counter_with_labels(num_labels: usize) -> Counter<'static> {
    let mut strings = HashMap::new();
    let mut ints = HashMap::new();
    let mut bools = HashMap::new();
    let mut floats = HashMap::new();

    for i in 0..num_labels {
        match i % 4 {
            0 => {
                strings.insert(
                    &*Box::leak(format!("str_label_{}", i).into_boxed_str()),
                    Cow::Borrowed(if i == 4 {
                        "value542134234134324523423f2kr2423k4l234l32k423kl43kl343k2432k32"
                    } else {
                        &*Box::leak(format!("value_{}", i).into_boxed_str())
                    }),
                );
            }
            1 => {
                ints.insert(
                    &*Box::leak(format!("int_label_{}", i).into_boxed_str()),
                    (i as i64) * 100,
                );
            }
            2 => {
                bools.insert(
                    &*Box::leak(format!("bool_label_{}", i).into_boxed_str()),
                    i % 2 == 0,
                );
            }
            3 => {
                floats.insert(
                    &*Box::leak(format!("float_label_{}", i).into_boxed_str()),
                    (i as f64) * std::f64::consts::PI,
                );
            }
            _ => unreachable!(),
        }
    }

    let labels = Labels {
        strings,
        ints,
        bools,
        floats,
    };

    Counter {
        name: "benchmark_counter",
        value: 42.0,
        timestamp: 1234567890,
        tid: 123,
        pid: 456,
        labels: Cow::Owned(labels),
        unit: Some("unit")
    }
}

#[divan::bench(args = [0, 2, 5, 10, 20])]
fn serialize_counter(bencher: Bencher, num_labels: usize) {
    let counter = create_counter_with_labels(num_labels);
    let required_size = compute_length(&counter).unwrap();
    let mut buf = vec![0u8; required_size];
    bencher.bench_local(|| {
        for _ in 0..1000 {
            serialize_to_buf(&counter, &mut buf).unwrap();
            black_box(&buf);
        }
    });
}

#[divan::bench(args = [0, 2, 5, 10, 20])]
fn access_counter(bencher: Bencher, num_labels: usize) {
    let counter = create_counter_with_labels(num_labels);
    let mut buf = AlignedVec::<8>::new();
    to_bytes_in::<_, rkyv::rancor::Error>(&counter, &mut buf).unwrap();

    bencher.bench(|| {
        for _ in 0..1000 {
            let archived =
                rkyv::access::<protocol::ArchivedCounter, rkyv::rancor::Error>(&buf).unwrap();
            black_box(archived);
        }
    })
}

fn main() {
    divan::main();
}
