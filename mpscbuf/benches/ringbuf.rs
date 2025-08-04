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

use std::hint::black_box;

use mpscbuf::{Consumer, Producer, WakeupStrategy};

fn main() {
    divan::main();
}

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

const BUFFER_SIZE: usize = 8 * 1024 * 1024 * 1024;

fn setup_ringbuf_with_size(wakeup_strategy: WakeupStrategy) -> (Producer, Consumer) {
    let consumer = Consumer::new(BUFFER_SIZE).unwrap();

    let memory_fd = consumer.memory_fd().try_clone_to_owned().unwrap();
    let notification_fd = consumer.notification_fd().try_clone_to_owned().unwrap();

    let producer = Producer::new(memory_fd, notification_fd, BUFFER_SIZE, wakeup_strategy).unwrap();

    (producer, consumer)
}

#[divan::bench(
    threads = [1, 2, 4, 8],
    args = [
        (64, WakeupStrategy::Forced), (1024, WakeupStrategy::Forced),
        (8, WakeupStrategy::NoWakeup), (64, WakeupStrategy::NoWakeup), (1024, WakeupStrategy::NoWakeup)
    ]
)]
fn bench_producer_speed(bencher: divan::Bencher, (record_size, wakeup): (usize, WakeupStrategy)) {
    let record = vec![0u8; record_size];
    bencher
        .with_inputs(|| setup_ringbuf_with_size(wakeup))
        .bench_values(|(producer, _consumer)| {
            let total_records = 10000;

            for _ in 0..total_records {
                let mut reserved = producer.reserve(record_size).unwrap();
                reserved.copy_from_slice(&record);
                black_box(reserved);
            }
        });
}

#[divan::bench(
    min_time = 1,
    args = [8]
)]
fn bench_single_reserve(bencher: divan::Bencher, record_size: usize) {
    let (producer, _consumer) = setup_ringbuf_with_size(WakeupStrategy::NoWakeup);
    let data = vec![0u8; record_size];
    bencher.bench_local(move || {
        for _ in 0..1000 {
            let mut reserved = producer.reserve(record_size).unwrap();
            reserved.copy_from_slice(&data);
            black_box(reserved);
        }
    });
}
