use mpscbuf::{Consumer, Producer, WakeupStrategy};

fn main() {
    divan::main();
}

const BUFFER_SIZE: usize = 2 * 1024 * 1024;

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
        (64, WakeupStrategy::Forced), (256, WakeupStrategy::Forced), (512, WakeupStrategy::Forced), (1024, WakeupStrategy::Forced),
        (64, WakeupStrategy::NoWakeup), (256, WakeupStrategy::NoWakeup), (512, WakeupStrategy::NoWakeup), (1024, WakeupStrategy::NoWakeup)
    ]
)]
fn bench_producer_speed(bencher: divan::Bencher, (record_size, wakeup): (usize, WakeupStrategy)) {
    let record = vec![0u8; record_size];
    bencher
        .with_inputs(|| setup_ringbuf_with_size(wakeup))
        .bench_values(|(producer, _consumer)| {
            let total_records = 1000;

            for _ in 0..total_records {
                let mut reserved = producer.reserve(record_size).unwrap();
                reserved.copy_from_slice(&record);
            }
        });
}
