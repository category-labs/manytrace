#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "Dual BSD/GPL";

#ifndef PERF_MAX_STACK_DEPTH
#define PERF_MAX_STACK_DEPTH 127
#endif

struct perf_sample_event {
    __u64 timestamp;
    __u32 tgid;
    __u32 pid;
    __u32 cpu_id;
    __s32 ustack_id;
    __s32 kstack_id;
};

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 256 * 1024);
} events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_STACK_TRACE);
    __uint(key_size, sizeof(u32));
    __uint(value_size, PERF_MAX_STACK_DEPTH * sizeof(u64));
    __uint(max_entries, 10000);
} stackmap SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __type(key, __u32);
    __type(value, __u8);
    __uint(max_entries, 1024);
} filter_tgid SEC(".maps");

const volatile struct {
    bool filter_tgid;
    bool collect_ustack;
    bool collect_kstack;
} cfg = {
    .filter_tgid = false,
    .collect_ustack = true,
    .collect_kstack = false,
};

static __always_inline int apply_tgid_filter(__u32 tgid) {
    if (!cfg.filter_tgid) {
        return 0;
    }
    __u8 *val = bpf_map_lookup_elem(&filter_tgid, &tgid);
    return val ? 0 : 1;
}

SEC("perf_event")
int profiler_perf_event(struct bpf_perf_event_data *ctx) {
    __u64 pid_tgid = bpf_get_current_pid_tgid();
    __u32 tgid = pid_tgid >> 32;
    __u32 pid = pid_tgid;
    
    if (pid == 0) {
        return 0;
    }
    
    if (apply_tgid_filter(tgid) > 0) {
        return 0;
    }
    
    struct perf_sample_event *event;
    event = bpf_ringbuf_reserve(&events, sizeof(*event), 0);
    if (!event) {
        return 0;
    }
    
    event->timestamp = bpf_ktime_get_ns();
    event->tgid = tgid;
    event->pid = pid;
    event->cpu_id = bpf_get_smp_processor_id();
    
    if (cfg.collect_ustack) {
        event->ustack_id = bpf_get_stackid(ctx, &stackmap, 
            BPF_F_USER_STACK | BPF_F_FAST_STACK_CMP | BPF_F_REUSE_STACKID);
    } else {
        event->ustack_id = -1;
    }
    
    if (cfg.collect_kstack) {
        event->kstack_id = bpf_get_stackid(ctx, &stackmap,
            BPF_F_FAST_STACK_CMP | BPF_F_REUSE_STACKID);
    } else {
        event->kstack_id = -1;
    }
    
    bpf_ringbuf_submit(event, 0);
    return 0;
}