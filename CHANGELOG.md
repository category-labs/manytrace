# Changelog

All notable changes to this project will be documented in this file.

Release versions follow Semantic Versioning:
https://semver.org/spec/v2.0.0.html

## [Unreleased]

## [0.1.3] - 2026-10-14

Add network and block I/O profiling to correlate application spans with network
traffic and disk activity in Perfetto. Scheduler tracing now distinguishes CPU
execution, scheduling delays, and resource waits.

#### Network profiling

Add TCP and UDP monitoring for successful payload bytes, operation counts, and
errors. Collect host totals or enable process and peer breakdowns. Set
`scaled = true` to report bits per second instead of bytes per interval.

```toml
[bpf.nettrack]
frequency = 5
peer_frequency = 1
scaled = true
protocols = ["tcp", "udp"]
directions = ["send", "receive"]
scopes = ["host"] # also: "process", "peer"
metrics = ["bytes"] # also: "operations", "errors"
```

Process-name filtering uses `filter_process` and requires `bpf.thread_tracker`.
Bound process and peer collection with `max_process_entries`, `max_peer_entries`,
and `max_peer_tracks`. Counters are sampled from cumulative BPF maps without
periodic per-CPU interrupts or a ring buffer.

For workloads that start immediately when the user agent connects, set
`pid_filters` before capture to retain the first network events. Process-name
filters take effect after the initial thread-tracker scan and can miss startup
traffic.

#### Block I/O profiling

Add per-device throughput, IOPS, errors, queue depth, busy time, and queue,
service, and total latency. Optional request timelines show individual I/O
operations alongside application spans.

```toml
[bpf.block_io]
frequency = 10
operations = ["read", "write"] # also: "flush", "discard"
metrics = ["throughput", "iops", "latency", "saturation", "errors"]
devices = ["nvme0n1"] # optional
timeline = false
```

Enable request spans with `timeline = true`. Control collection with
`timeline_sample_every`, `timeline_min_latency_us`, `max_requests`, and `ringbuf`.
Aggregate counters remain unsampled. Throughput reflects completed block-device
activity, including buffered writes when they reach the block layer.

#### Scheduler tracing

Distinguish `running`, `runnable`, and `blocked` states. Runnable spans identify
wakeup and preemption reasons; blocked spans include task state and kernel
symbols when available. Rename the per-thread `kernel` track to `scheduler` and
replace the `waiting` and `waking` span names with `blocked` and `runnable`.

```toml
[bpf.schedtrace]
filter_process = []
```

#### Capture reliability

Fix starvation that could prevent duration-limited captures from stopping under
sustained event load. Flush final network and block I/O counters during graceful
shutdown to retain activity since the last sample.

#### Examples and cleanup

Add `net_demo`, `disk_demo`, and `scheduler_demo` workloads showing application
spans alongside network traffic, direct disk I/O, and CPU contention. Remove the
unused Chrome trace-format crate; traces continue to use Perfetto format. Add
source license headers and automated license checks in CI.

## [0.1.1] - 2025-07-11

#### Initial release

Initial release with core functionality.
