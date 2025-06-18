#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

struct cpu_state {
    u32 tid;
    u32 tgid;
    u64 start_time;
    u64 kernel_start_time;
    u64 kernel_time_acc;
    u32 in_kernel;
};

struct cpu_event {
    u32 tid;
    u32 tgid;
    u64 total_time_ns;
    u64 kernel_time_ns;
    u32 cpu;
    u64 timestamp;
};

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, u32);
    __type(value, struct cpu_state);
} cpu_states SEC(".maps");

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

static __always_inline u64 get_time() {
    return bpf_ktime_get_ns();
}

static __always_inline bool should_track_tgid(u32 tgid) {
    if (tgid == 0) {
        return true;
    }
    u32 *val = bpf_map_lookup_elem(&tracked_tgids, &tgid);
    if (!val) {
        return false;
    }
    return true;
}

static __always_inline void submit_cpu_event(struct cpu_state *state, u64 end_time) {
    struct cpu_event *e;
    
    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return;
    }
    
    u64 kernel_time = state->kernel_time_acc;
    if (state->in_kernel && state->kernel_start_time > 0) {
        kernel_time += (end_time - state->kernel_start_time);
    }
    
    e->tid = state->tid;
    e->tgid = state->tgid;
    e->total_time_ns = end_time - state->start_time;
    e->kernel_time_ns = kernel_time;
    e->cpu = bpf_get_smp_processor_id();
    e->timestamp = state->start_time;
    
    bpf_ringbuf_submit(e, 0);
}

SEC("tp/sched/sched_switch")
int handle_sched_switch(struct trace_event_raw_sched_switch *ctx)
{
    u64 now = get_time();
    u32 zero = 0;
    struct cpu_state *state = bpf_map_lookup_elem(&cpu_states, &zero);
    if (!state) {
        return 0;
    }
    
    u32 prev_pid = ctx->prev_pid;
    u32 next_pid = ctx->next_pid;
    
    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 tgid = pid_tgid >> 32;
    state->tgid = tgid;
    
    if (state->tid == prev_pid && state->tid != 0) {
        if (should_track_tgid(state->tgid)) {
            submit_cpu_event(state, now);
        }
    }
    
    state->tid = next_pid;
    state->tgid = 0;
    state->start_time = now;
    state->kernel_start_time = 0;
    state->kernel_time_acc = 0;
    state->in_kernel = 0;
    
    return 0;
}

SEC("tp/raw_syscalls/sys_enter")
int handle_sys_enter(struct trace_event_raw_sys_enter *ctx)
{
    u32 zero = 0;
    struct cpu_state *state = bpf_map_lookup_elem(&cpu_states, &zero);
    if (!state) {
        return 0;
    }
    
    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 tid = pid_tgid;
    
    if (state->tid != tid) {
        return 0;
    }
    
    if (!state->in_kernel) {
        state->in_kernel = 1;
        state->kernel_start_time = get_time();
    }
    
    return 0;
}

SEC("tp/raw_syscalls/sys_exit")
int handle_sys_exit(struct trace_event_raw_sys_exit *ctx)
{
    u32 zero = 0;
    struct cpu_state *state = bpf_map_lookup_elem(&cpu_states, &zero);
    if (!state) {
        return 0;
    }
    
    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 tid = pid_tgid;
    
    if (state->tid != tid) {
        return 0;
    }
    
    if (state->in_kernel && state->kernel_start_time > 0) {
        u64 now = get_time();
        state->kernel_time_acc += (now - state->kernel_start_time);
        state->kernel_start_time = 0;
    }
    
    state->in_kernel = 0;
    
    return 0;
}

SEC("perf_event")
int handle_boundary_event(void *ctx)
{
    u32 zero = 0;
    struct cpu_state *state = bpf_map_lookup_elem(&cpu_states, &zero);
    if (!state) {
        return 0;
    }
    
    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 tid = pid_tgid;
    u32 tgid = pid_tgid >> 32;
    
    if (state->tid != tid || tid == 0 || !should_track_tgid(tgid)) {
        return 0;
    }

    u64 now = get_time();
    state->tgid = tgid;
    submit_cpu_event(state, now);
    
    state->start_time = now;
    if (state->in_kernel) {
        state->kernel_start_time = now;
    }
    state->kernel_time_acc = 0;
    
    return 0;
}

