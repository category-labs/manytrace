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

#include "vmlinux.h"
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "GPL";

#define HISTOGRAM_BUCKETS 32
#define TASK_COMM_LEN 16
#define REQ_OP_MASK 0xff

enum block_operation {
    BLOCK_OP_READ = 0,
    BLOCK_OP_WRITE = 1,
    BLOCK_OP_FLUSH = 2,
    BLOCK_OP_DISCARD = 3,
    BLOCK_OP_OTHER = 4,
    BLOCK_OP_COUNT = 5,
};

enum request_operation {
    REQ_OP_READ_VALUE = 0,
    REQ_OP_WRITE_VALUE = 1,
    REQ_OP_FLUSH_VALUE = 2,
    REQ_OP_DISCARD_VALUE = 3,
    REQ_OP_SECURE_ERASE_VALUE = 5,
    REQ_OP_WRITE_ZEROES_VALUE = 9,
    REQ_OP_ZONE_APPEND_VALUE = 13,
};

// The repository's portable vmlinux.h intentionally leaves request opaque.
// The ___local suffix asks CO-RE to match this flavor against struct request.
struct request___local {
    struct request_queue *q;
    blk_opf_t cmd_flags;
    unsigned int __data_len;
    sector_t __sector;
    struct block_device *part;
} __attribute__((preserve_access_index));

struct operation_stats {
    u64 bytes;
    u64 operations;
    u64 errors;
    u64 queue_latency_ns;
    u64 service_latency_ns;
    u64 total_latency_ns;
    u64 service_histogram[HISTOGRAM_BUCKETS];
    u64 total_histogram[HISTOGRAM_BUCKETS];
};

struct device_stats {
    struct operation_stats operations[BLOCK_OP_COUNT];
    u64 requeues;
    u64 lost_requests;
    u64 timeline_drops;
    u64 first_seen_ns;
    u64 last_seen_ns;
};

struct device_depth {
    struct bpf_spin_lock lock;
    u32 padding;
    u64 first_seen_ns;
    u64 last_update_ns;
    u64 busy_ns;
    u64 saturated_ns;
    u64 inflight_ns;
    u64 queued_ns;
    u32 inflight;
    u32 queued;
    u32 max_inflight;
    u32 max_queued;
};

struct request_state {
    u64 first_seen_ns;
    u64 insert_ns;
    u64 issue_ns;
    u64 service_ns;
    u64 sector;
    u32 bytes;
    u32 remaining_bytes;
    u32 dev;
    u32 partition_dev;
    u32 tgid;
    u32 pid;
    u32 requeues;
    u8 operation;
    u8 inserted;
    u8 issued;
    u8 padding;
    char comm[TASK_COMM_LEN];
};

struct request_span_event {
    u64 request_id;
    u64 start_ns;
    u64 end_ns;
    u64 queue_ns;
    u64 service_ns;
    u64 total_ns;
    u64 sector;
    u32 dev;
    u32 partition_dev;
    u32 bytes;
    u32 tgid;
    u32 pid;
    u32 requeues;
    s32 error;
    u8 operation;
    u8 completed;
    u8 padding[6];
    char comm[TASK_COMM_LEN];
};

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 64);
    __type(key, u32);
    __type(value, struct device_stats);
} device_stats SEC(".maps");

// A never-mutated zero value avoids placing the relatively large histogram
// value on the 512-byte BPF stack when a device is first observed.
struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, u32);
    __type(value, struct device_stats);
} zero_device_stats SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 64);
    __type(key, u32);
    __type(value, struct device_depth);
} device_depths SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 32768);
    __type(key, u64);
    __type(value, struct request_state);
} requests SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 64);
    __type(key, u32);
    __type(value, u32);
} tracked_devices SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 8 * 1024 * 1024);
} events SEC(".maps");

const volatile struct {
    u32 operation_mask;
    u32 timeline_sample_every;
    u64 timeline_min_latency_ns;
    bool bytes_enabled;
    bool operations_enabled;
    bool latency_enabled;
    bool saturation_enabled;
    bool errors_enabled;
    bool timeline_enabled;
    bool filter_enabled;
    bool request_tracking_enabled;
} cfg = {
    .operation_mask = (1U << BLOCK_OP_READ) | (1U << BLOCK_OP_WRITE),
    .timeline_sample_every = 1,
    .timeline_min_latency_ns = 0,
    .bytes_enabled = true,
    .operations_enabled = true,
    .latency_enabled = true,
    .saturation_enabled = true,
    .errors_enabled = true,
    .timeline_enabled = false,
    .filter_enabled = false,
    .request_tracking_enabled = true,
};

static __always_inline u8 request_operation(struct request *rq)
{
    struct request___local *request = (void *)rq;
    u32 command = BPF_CORE_READ(request, cmd_flags) & REQ_OP_MASK;
    if (command == REQ_OP_READ_VALUE)
        return BLOCK_OP_READ;
    if (command == REQ_OP_WRITE_VALUE || command == REQ_OP_WRITE_ZEROES_VALUE
        || command == REQ_OP_ZONE_APPEND_VALUE)
        return BLOCK_OP_WRITE;
    if (command == REQ_OP_FLUSH_VALUE)
        return BLOCK_OP_FLUSH;
    if (command == REQ_OP_DISCARD_VALUE || command == REQ_OP_SECURE_ERASE_VALUE)
        return BLOCK_OP_DISCARD;
    return BLOCK_OP_OTHER;
}

static __always_inline u32 request_device(struct request *rq)
{
    struct request___local *request = (void *)rq;
    struct gendisk *disk = BPF_CORE_READ(request, q, disk);
    if (!disk)
        return 0;
    struct block_device *part0 = BPF_CORE_READ(disk, part0);
    if (part0)
        return BPF_CORE_READ(part0, bd_dev);
    return 0;
}

static __always_inline u32 request_partition(struct request *rq)
{
    struct request___local *request = (void *)rq;
    struct block_device *part = BPF_CORE_READ(request, part);
    if (part)
        return BPF_CORE_READ(part, bd_dev);
    return request_device(rq);
}

static __always_inline bool should_track(u32 dev, u8 operation)
{
    if (!dev || operation >= BLOCK_OP_COUNT)
        return false;
    if (!(cfg.operation_mask & (1U << operation)))
        return false;
    if (!cfg.filter_enabled)
        return true;
    return bpf_map_lookup_elem(&tracked_devices, &dev) != NULL;
}

static __always_inline struct device_stats *get_device_stats(u32 dev, u64 now)
{
    struct device_stats *stats = bpf_map_lookup_elem(&device_stats, &dev);
    if (stats) {
        if (!stats->first_seen_ns)
            __sync_val_compare_and_swap(&stats->first_seen_ns, 0, now);
        return stats;
    }

    u32 zero = 0;
    struct device_stats *initial = bpf_map_lookup_elem(&zero_device_stats, &zero);
    if (!initial)
        return NULL;
    bpf_map_update_elem(&device_stats, &dev, initial, BPF_NOEXIST);
    stats = bpf_map_lookup_elem(&device_stats, &dev);
    if (stats) {
        __sync_val_compare_and_swap(&stats->first_seen_ns, 0, now);
        __sync_lock_test_and_set(&stats->last_seen_ns, now);
    }
    return stats;
}

static __always_inline struct device_depth *get_device_depth(u32 dev)
{
    return bpf_map_lookup_elem(&device_depths, &dev);
}

static __always_inline void adjust_depth(u32 dev, int queued_delta,
                                         int inflight_delta, u64 now)
{
    if (!cfg.saturation_enabled)
        return;
    struct device_depth *depth = get_device_depth(dev);
    if (!depth)
        return;

    bpf_spin_lock(&depth->lock);
    if (depth->last_update_ns && now > depth->last_update_ns) {
        u64 elapsed = now - depth->last_update_ns;
        if (depth->inflight)
            depth->busy_ns += elapsed;
        if (depth->queued)
            depth->saturated_ns += elapsed;
        depth->inflight_ns += elapsed * depth->inflight;
        depth->queued_ns += elapsed * depth->queued;
    }

    if (queued_delta > 0)
        depth->queued++;
    else if (queued_delta < 0 && depth->queued)
        depth->queued--;
    if (inflight_delta > 0)
        depth->inflight++;
    else if (inflight_delta < 0 && depth->inflight)
        depth->inflight--;
    if (depth->queued > depth->max_queued)
        depth->max_queued = depth->queued;
    if (depth->inflight > depth->max_inflight)
        depth->max_inflight = depth->inflight;
    depth->last_update_ns = now;
    bpf_spin_unlock(&depth->lock);
}

static __always_inline u32 latency_bucket(u64 latency_ns)
{
    u64 micros = latency_ns / 1000;
    if (micros <= 1)
        return 0;
    if (micros >= (1ULL << (HISTOGRAM_BUCKETS - 1)))
        return HISTOGRAM_BUCKETS - 1;
    u32 bucket = 0;
    if (micros >= (1ULL << 16)) {
        micros >>= 16;
        bucket += 16;
    }
    if (micros >= (1ULL << 8)) {
        micros >>= 8;
        bucket += 8;
    }
    if (micros >= (1ULL << 4)) {
        micros >>= 4;
        bucket += 4;
    }
    if (micros >= (1ULL << 2)) {
        micros >>= 2;
        bucket += 2;
    }
    if (micros >= (1ULL << 1))
        bucket += 1;
    return bucket;
}

static __always_inline void record_bytes(u32 dev, u8 operation, u32 bytes,
                                         int error, u64 now)
{
    if (operation >= BLOCK_OP_COUNT)
        return;
    if (!cfg.bytes_enabled && (!cfg.errors_enabled || !error))
        return;
    struct device_stats *stats = get_device_stats(dev, now);
    if (!stats)
        return;
    __sync_lock_test_and_set(&stats->last_seen_ns, now);
    if (cfg.bytes_enabled && bytes)
        __sync_fetch_and_add(&stats->operations[operation].bytes, bytes);
    if (cfg.errors_enabled && error)
        __sync_fetch_and_add(&stats->operations[operation].errors, 1);
}

static __always_inline void record_completion(u32 dev, u8 operation, u64 queue_ns,
                                              u64 service_ns, u64 total_ns, u64 now)
{
    if (operation >= BLOCK_OP_COUNT)
        return;
    if (!cfg.operations_enabled && !cfg.latency_enabled)
        return;
    struct device_stats *stats = get_device_stats(dev, now);
    if (!stats)
        return;
    __sync_lock_test_and_set(&stats->last_seen_ns, now);
    if (cfg.operations_enabled)
        __sync_fetch_and_add(&stats->operations[operation].operations, 1);
    if (!cfg.latency_enabled)
        return;
    __sync_fetch_and_add(&stats->operations[operation].queue_latency_ns, queue_ns);
    __sync_fetch_and_add(&stats->operations[operation].service_latency_ns, service_ns);
    __sync_fetch_and_add(&stats->operations[operation].total_latency_ns, total_ns);
    u32 service_bucket = latency_bucket(service_ns);
    u32 total_bucket = latency_bucket(total_ns);
    __sync_fetch_and_add(&stats->operations[operation].service_histogram[service_bucket], 1);
    __sync_fetch_and_add(&stats->operations[operation].total_histogram[total_bucket], 1);
}

static __always_inline void record_lost_request(u32 dev, u64 now)
{
    if (!cfg.errors_enabled)
        return;
    struct device_stats *stats = get_device_stats(dev, now);
    if (stats)
        __sync_fetch_and_add(&stats->lost_requests, 1);
}

static __always_inline void fill_owner(struct request_state *state)
{
    u64 pid_tgid = bpf_get_current_pid_tgid();
    state->tgid = pid_tgid >> 32;
    state->pid = (u32)pid_tgid;
    bpf_get_current_comm(state->comm, sizeof(state->comm));
}

static __always_inline int create_request(struct request *rq, u64 request_id,
                                          u32 dev, u32 partition_dev,
                                          u8 operation, u64 now, bool inserted)
{
    struct request_state initial;
    __builtin_memset(&initial, 0, sizeof(initial));
    initial.first_seen_ns = now;
    initial.insert_ns = inserted ? now : 0;
    struct request___local *request = (void *)rq;
    initial.sector = BPF_CORE_READ(request, __sector);
    initial.bytes = BPF_CORE_READ(request, __data_len);
    initial.remaining_bytes = initial.bytes;
    initial.dev = dev;
    initial.partition_dev = partition_dev;
    initial.operation = operation;
    initial.inserted = inserted;
    fill_owner(&initial);

    if (bpf_map_update_elem(&requests, &request_id, &initial, BPF_NOEXIST) != 0) {
        record_lost_request(dev, now);
        return -1;
    }
    if (inserted)
        adjust_depth(dev, 1, 0, now);
    return 0;
}

static __always_inline void emit_request_span(u64 request_id,
                                              struct request_state *state,
                                              u64 start_ns, u64 end_ns,
                                              u64 queue_ns, u64 service_ns,
                                              u64 total_ns, int error,
                                              bool completed)
{
    if (!cfg.timeline_enabled || end_ns <= start_ns
        || total_ns < cfg.timeline_min_latency_ns)
        return;
    if (cfg.timeline_sample_every > 1
        && (bpf_get_prandom_u32() % cfg.timeline_sample_every) != 0)
        return;

    struct request_span_event *event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
    if (!event) {
        struct device_stats *stats = get_device_stats(state->dev, end_ns);
        if (stats)
            __sync_fetch_and_add(&stats->timeline_drops, 1);
        return;
    }
    event->request_id = request_id;
    event->start_ns = start_ns;
    event->end_ns = end_ns;
    event->queue_ns = queue_ns;
    event->service_ns = service_ns;
    event->total_ns = total_ns;
    event->sector = state->sector;
    event->dev = state->dev;
    event->partition_dev = state->partition_dev;
    event->bytes = state->bytes;
    event->tgid = state->tgid;
    event->pid = state->pid;
    event->requeues = state->requeues;
    event->error = error;
    event->operation = state->operation;
    event->completed = completed;
    __builtin_memcpy(event->comm, state->comm, sizeof(event->comm));
    __builtin_memset(event->padding, 0, sizeof(event->padding));
    bpf_ringbuf_submit(event, 0);
}

SEC("tp_btf/block_rq_insert")
int BPF_PROG(trace_block_rq_insert, struct request *rq)
{
    if (!cfg.request_tracking_enabled)
        return 0;
    u32 dev = request_device(rq);
    u8 operation = request_operation(rq);
    if (!should_track(dev, operation))
        return 0;

    u64 now = bpf_ktime_get_ns();
    u64 request_id = (u64)rq;
    struct request_state *state = bpf_map_lookup_elem(&requests, &request_id);
    if (!state) {
        create_request(rq, request_id, dev, request_partition(rq), operation, now, true);
        return 0;
    }

    struct request___local *request = (void *)rq;
    state->bytes = BPF_CORE_READ(request, __data_len);
    if (state->remaining_bytes < state->bytes)
        state->remaining_bytes = state->bytes;
    if (!state->inserted && !state->issued) {
        state->insert_ns = now;
        state->inserted = true;
        adjust_depth(dev, 1, 0, now);
    }
    return 0;
}

SEC("tp_btf/block_rq_merge")
int BPF_PROG(trace_block_rq_merge, struct request *rq)
{
    if (!cfg.request_tracking_enabled)
        return 0;
    u64 request_id = (u64)rq;
    struct request_state *state = bpf_map_lookup_elem(&requests, &request_id);
    if (state) {
        struct request___local *request = (void *)rq;
        state->bytes = BPF_CORE_READ(request, __data_len);
        state->remaining_bytes = state->bytes;
    }
    return 0;
}

SEC("tp_btf/block_rq_issue")
int BPF_PROG(trace_block_rq_issue, struct request *rq)
{
    if (!cfg.request_tracking_enabled)
        return 0;
    u32 dev = request_device(rq);
    u8 operation = request_operation(rq);
    if (!should_track(dev, operation))
        return 0;

    u64 now = bpf_ktime_get_ns();
    u64 request_id = (u64)rq;
    struct request_state *state = bpf_map_lookup_elem(&requests, &request_id);
    if (!state) {
        if (create_request(rq, request_id, dev, request_partition(rq), operation, now, false))
            return 0;
        state = bpf_map_lookup_elem(&requests, &request_id);
        if (!state)
            return 0;
    }

    if (state->inserted) {
        adjust_depth(dev, -1, 0, now);
        state->inserted = false;
    }
    if (!state->issued) {
        adjust_depth(dev, 0, 1, now);
        state->issued = true;
    }
    state->issue_ns = now;
    struct request___local *request = (void *)rq;
    state->bytes = BPF_CORE_READ(request, __data_len);
    if (!state->remaining_bytes)
        state->remaining_bytes = state->bytes;
    return 0;
}

SEC("tp_btf/block_rq_requeue")
int BPF_PROG(trace_block_rq_requeue, struct request *rq)
{
    if (!cfg.errors_enabled && !cfg.request_tracking_enabled)
        return 0;
    u64 now = bpf_ktime_get_ns();
    u32 dev = request_device(rq);
    u8 operation = request_operation(rq);
    if (!should_track(dev, operation))
        return 0;

    if (cfg.errors_enabled) {
        struct device_stats *stats = get_device_stats(dev, now);
        if (stats)
            __sync_fetch_and_add(&stats->requeues, 1);
    }
    if (!cfg.request_tracking_enabled)
        return 0;

    u64 request_id = (u64)rq;
    struct request_state *state = bpf_map_lookup_elem(&requests, &request_id);
    if (!state)
        return 0;

    u64 attempt_ns = 0;
    if (state->issued && now > state->issue_ns) {
        attempt_ns = now - state->issue_ns;
        state->service_ns += attempt_ns;
        adjust_depth(state->dev, 0, -1, now);
        state->issued = false;
    }
    state->requeues++;
    emit_request_span(request_id, state, state->issue_ns, now,
                      0, attempt_ns, now - state->first_seen_ns, 0, false);
    state->issue_ns = 0;
    return 0;
}

SEC("tp_btf/block_rq_complete")
int BPF_PROG(trace_block_rq_complete, struct request *rq, blk_status_t error,
             unsigned int nr_bytes)
{
    u32 dev = request_device(rq);
    u8 operation = request_operation(rq);
    if (!should_track(dev, operation))
        return 0;

    u64 now = bpf_ktime_get_ns();
    record_bytes(dev, operation, nr_bytes, error, now);
    if (!cfg.request_tracking_enabled)
        return 0;

    u64 request_id = (u64)rq;
    struct request_state *state = bpf_map_lookup_elem(&requests, &request_id);
    if (!state) {
        record_completion(dev, operation, 0, 0, 0, now);
        return 0;
    }

    bool complete = error || nr_bytes >= state->remaining_bytes;
    if (!complete) {
        state->remaining_bytes -= nr_bytes;
        return 0;
    }

    if (state->issued && now > state->issue_ns) {
        state->service_ns += now - state->issue_ns;
        adjust_depth(state->dev, 0, -1, now);
        state->issued = false;
    }
    if (state->inserted) {
        adjust_depth(state->dev, -1, 0, now);
        state->inserted = false;
    }

    u64 total_ns = now > state->first_seen_ns ? now - state->first_seen_ns : 0;
    u64 queue_ns = total_ns > state->service_ns ? total_ns - state->service_ns : 0;
    record_completion(state->dev, state->operation, queue_ns,
                      state->service_ns, total_ns, now);
    u64 span_start_ns = state->issue_ns ? state->issue_ns : state->first_seen_ns;
    emit_request_span(request_id, state, span_start_ns, now,
                      queue_ns, state->service_ns, total_ns, error, true);
    bpf_map_delete_elem(&requests, &request_id);
    return 0;
}
