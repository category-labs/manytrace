#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

struct sched_event {
    u32 pid;
    u32 tid;
    u64 timestamp;
    u32 cpu;
};

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
        return true;
    }
    u32 *val = bpf_map_lookup_elem(&tracked_tgids, &tgid);
    if (!val) {
        return false;
    }
    return true;
}

SEC("tp/sched/sched_switch")
int handle_sched_switch(struct trace_event_raw_sched_switch *ctx)
{
    return 0;
}

SEC("tp/sched/sched_waking")
int handle_sched_waking(struct trace_event_raw_sched_waking *ctx)
{
    return 0;
}