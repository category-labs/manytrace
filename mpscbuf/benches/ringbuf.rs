use mpscbuf::{Consumer, Notification, Producer, RingBuf, WakeupStrategy};

fn main() {
    divan::main();
}

const BUFFER_SIZE: usize = 8 * 1024 * 1024;

fn setup_ringbuf_with_size(wakeup_stragy: WakeupStrategy) -> (Producer, Consumer) {
    let ringbuf1 = RingBuf::new(BUFFER_SIZE).unwrap();
    let ringbuf2 = RingBuf::from_fd(ringbuf1.clone_fd().unwrap(), BUFFER_SIZE).unwrap();
    let notification1 = Notification::new().unwrap();
    let notification2 =
        unsafe { Notification::from_owned_fd(notification1.fd().try_clone_to_owned().unwrap()) };

    let consumer = Consumer::new(ringbuf1, notification1);
    let producer = Producer::with_wakeup_strategy(ringbuf2, notification2, wakeup_stragy);

    (producer, consumer)
}

#[divan::bench(
    threads = [1, 2, 4, 8],
    args = [
        (64, WakeupStrategy::Forced), (256, WakeupStrategy::Forced), (512, WakeupStrategy::Forced), (1024, WakeupStrategy::Forced),
        (64, WakeupStrategy::SelfPacing), (256, WakeupStrategy::SelfPacing), (512, WakeupStrategy::SelfPacing), (1024, WakeupStrategy::SelfPacing)
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
