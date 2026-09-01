# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `nettrack` BPF module for network I/O monitoring
  Config options:
  `frequency`: sampling frequency in Hz
  `scaled`: report rates in bits/s instead of interval bytes
- Configurable `nettrack` protocols, directions, metrics, and host/process/peer scopes
- Bounded per-process and per-peer network aggregation with independent peer sampling frequency
- `block_io` BPF module with per-device throughput, IOPS, latency distributions, errors,
  queue depth, busy/waiting time, and opt-in sampled request timelines
- Instrumented Rust direct-I/O example for correlating application spans and block-device activity

### Fixed
- Count successful TCP and UDP payload bytes instead of requested send sizes
- Replace `nettrack` perf-event and ring-buffer boundaries with cumulative map snapshots
- Flush final cumulative network and block-I/O map snapshots during graceful shutdown
- Spawn a separate thread for timer interrupt to fix starvation issues ([189c785](https://github.com/category-labs/manytrace/commit/189c785))

## [0.1.1] - 2025-07-11

### Added
- Initial release with core functionality
