#[cfg(all(test, feature = "loom"))]
mod tests {
    use crate::{
        producer::{Producer, WakeupStrategy},
        ringbuf::RingBuf,
        sync::notification::Notification,
        Consumer,
    };
    use loom::{model::Builder, thread};

    #[test]
    fn test_four_producers() {
        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(3);
        }

        builder.check(|| {
            let page_size = 4096;
            let size = page_size * 8;

            let ringbuf = RingBuf::new(size).unwrap();
            let ringbuf_clone = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size).unwrap();
            let notification = Notification::new().unwrap();

            let mut consumer = Consumer::from_parts(ringbuf, notification.clone());

            let num_producers = 3;
            let msgs_per_producer = 2;

            let mut handles = vec![];

            for producer_id in 0..num_producers {
                let ringbuf_clone =
                    RingBuf::from_fd(ringbuf_clone.clone_fd().unwrap(), size).unwrap();
                let notification_clone = notification.clone();
                let producer =
                    Producer::from_parts(ringbuf_clone, notification_clone, WakeupStrategy::Forced);

                let handle = thread::spawn(move || {
                    for i in 0..msgs_per_producer {
                        let data = format!("p{}_{}", producer_id, i);
                        if let Ok(mut reserved) = producer.reserve(data.len()) {
                            reserved.copy_from_slice(data.as_bytes());
                        }
                    }
                });

                handles.push(handle);
            }

            let mut count = 0;
            let mut producer_counts = vec![0; num_producers];
            for record in consumer.iter() {
                let data = std::str::from_utf8(record.as_slice()).unwrap();
                for (producer_id, count) in producer_counts.iter_mut().enumerate() {
                    if data.starts_with(&format!("p{}_", producer_id)) {
                        *count += 1;
                        break;
                    }
                }
                count += 1;
            }

            for handle in handles {
                handle.join().unwrap();
            }
            for record in consumer.iter() {
                let data = std::str::from_utf8(record.as_slice()).unwrap();
                for (producer_id, count) in producer_counts.iter_mut().enumerate() {
                    if data.starts_with(&format!("p{}_", producer_id)) {
                        *count += 1;
                        break;
                    }
                }
                count += 1;
            }

            assert_eq!(count, msgs_per_producer * num_producers);
            for (producer_id, count) in producer_counts.iter().enumerate() {
                assert_eq!(
                    *count, msgs_per_producer,
                    "Producer {} sent {} messages, expected {}",
                    producer_id, *count, msgs_per_producer
                );
            }
        });
    }

    #[test]
    fn test_single_producer_single_consumer() {
        let mut builder = Builder::new();
        if builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(3);
        }

        builder.check(|| {
            let page_size = 4096;
            let size = page_size * 4;

            let ringbuf = RingBuf::new(size).unwrap();
            let ringbuf_clone = RingBuf::from_fd(ringbuf.clone_fd().unwrap(), size).unwrap();
            let notification = Notification::new().unwrap();

            let mut consumer = Consumer::from_parts(ringbuf, notification.clone());
            let producer =
                Producer::from_parts(ringbuf_clone, notification, WakeupStrategy::Forced);

            let num_messages = 3;

            let producer_handle = thread::spawn(move || {
                for i in 0..num_messages {
                    let data = format!("message_{}", i);
                    if let Ok(mut reserved) = producer.reserve(data.len()) {
                        reserved.copy_from_slice(data.as_bytes());
                    }
                }
            });

            let mut received = vec![];
            while received.len() < num_messages {
                for record in consumer.iter() {
                    let data = std::str::from_utf8(record.as_slice()).unwrap();
                    received.push(data.to_string());
                    if received.len() >= num_messages {
                        break;
                    }
                }
                if received.len() < num_messages {
                    consumer.wait().unwrap();
                }
            }

            producer_handle.join().unwrap();

            assert_eq!(received.len(), num_messages);
            for i in 0..num_messages {
                assert!(received.contains(&format!("message_{}", i)));
            }
        });
    }
}
