#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

struct sched_span_event {
    u32 pid;
    u32 tid;
    u64 start_time;
    u64 end_time;
    u32 cpu;
};

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, u32);
    __type(value, u64);
} switch_start SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 256 * 1024);
} events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, u32);
    __type(value, u32);
} tracked_tgids SEC(".maps");

const volatile struct {
    bool filter_enabled;
} cfg = {
    .filter_enabled = false,
};

static __always_inline bool should_track_tgid(u32 tgid) {
    if (!cfg.filter_enabled) {
        return true;
    }
    if (tgid == 0) {
        return false;
    }
    u32 *val = bpf_map_lookup_elem(&tracked_tgids, &tgid);
    return val != NULL;
}

static __always_inline u64 record_start(u64 timestamp) {
    u32 zero = 0;
    u64 *start = bpf_map_lookup_elem(&switch_start, &zero);
    if (!start) {
        return 0;
    }
    
    u64 old = *start;
    *start = timestamp;
    return old;
}

SEC("tp_btf/sched_switch")
int handle_sched_switch(u64 *ctx)
{
    struct task_struct *prev = (struct task_struct *)ctx[1];
    u64 end = bpf_ktime_get_ns();
    u64 start = record_start(end);
    
    if (start == 0 || start > end) {
        return 0;
    }
    
    if (prev->pid == 0) {
        return 0;
    }
    
    u32 tgid = BPF_CORE_READ(prev, tgid);
    if (!should_track_tgid(tgid)) {
        return 0;
    }
    
    struct sched_span_event *e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return 0;
    }
    
    e->pid = tgid;
    e->tid = BPF_CORE_READ(prev, pid);
    e->start_time = start;
    e->end_time = end;
    e->cpu = bpf_get_smp_processor_id();
    
    bpf_ringbuf_submit(e, 0);
    return 0;
}

