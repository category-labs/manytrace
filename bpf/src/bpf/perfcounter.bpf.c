#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "GPL";

#define MAX_PERF_COUNTERS 8

struct perf_counter_event {
    __u64 timestamp;
    __u32 cpu_id;
    __u32 padding;
    __u64 counters[MAX_PERF_COUNTERS];
};

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 256 * 1024);
} events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_PERF_EVENT_ARRAY);
    __uint(key_size, sizeof(int));
    __uint(value_size, sizeof(int));
    __uint(max_entries, 1);
} perf_counters SEC(".maps");

const volatile struct {
    __u32 counter_count;
} cfg = {
    .counter_count = 1,
};

SEC("perf_event")
int perfcounter_timer(struct bpf_perf_event_data *ctx) {
    struct perf_counter_event *event;
    event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
    if (!event) {
        return 0;
    }
    
    event->timestamp = bpf_ktime_get_ns();
    event->cpu_id = bpf_get_smp_processor_id();
    
    __u32 cpu = event->cpu_id;
    __u32 counter_count = cfg.counter_count;
    
    if (counter_count > MAX_PERF_COUNTERS) {
        counter_count = MAX_PERF_COUNTERS;
    }
    
    #pragma unroll
    for (int i = 0; i < MAX_PERF_COUNTERS; i++) {
        if (i < counter_count) {
            struct bpf_perf_event_value val = {};
            int idx = cpu * counter_count + i;
            long ret = bpf_perf_event_read_value(&perf_counters, idx, &val, sizeof(val));
            if (ret == 0) {
                event->counters[i] = val.counter;
            } else {
                event->counters[i] = 0;
            }
        } else {
            event->counters[i] = 0;
        }
    }
    
    bpf_ringbuf_submit(event, 0);
    return 0;
}