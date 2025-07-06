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
    u64 end_time;
    u64 kernel_time_ns;
    u64 start_time;
    u32 cpu;
    bool boundary;
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

const volatile struct {
    bool filter_enabled;
} cfg = {
    .filter_enabled = false,
};

static __always_inline u64 get_time() {
    return bpf_ktime_get_ns();
}

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

static __always_inline void submit_cpu_event(struct cpu_state *state, u64 end_time, bool boundary) {
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
    e->end_time = end_time;
    e->kernel_time_ns = kernel_time;
    e->cpu = bpf_get_smp_processor_id();
    e->start_time = state->start_time;
    e->boundary = boundary;
    
    bpf_ringbuf_submit(e, 0);
}

SEC("tp_btf/sched_switch")
int BPF_PROG(handle_sched_switch, bool preempt, struct task_struct *prev, struct task_struct *next)
{
    u64 now = get_time();
    u32 zero = 0;
    struct cpu_state *state = bpf_map_lookup_elem(&cpu_states, &zero);
    if (!state) {
        return 0;
    }
    
    u32 prev_pid = BPF_CORE_READ(prev, pid);
    u32 next_pid = BPF_CORE_READ(next, pid);
    u32 prev_tgid = BPF_CORE_READ(prev, tgid);
    u32 next_tgid = BPF_CORE_READ(next, tgid);
    
    if (state->tid == prev_pid && state->tid != 0) {
        state->tgid = prev_tgid;
        if (should_track_tgid(state->tgid)) {
            submit_cpu_event(state, now, false);
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

SEC("tp_btf/sys_enter")
int BPF_PROG(handle_sys_enter, struct pt_regs *regs, long id)
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

SEC("tp_btf/sys_exit")
int BPF_PROG(handle_sys_exit, struct pt_regs *regs, long ret)
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
    
    if (should_track_tgid(tgid)) {
        state->tgid = tgid;
    }

    u64 now = get_time();
    submit_cpu_event(state, now, true);
    
    state->start_time = now;
    if (state->in_kernel) {
        state->kernel_start_time = now;
    }
    state->kernel_time_acc = 0;
    
    return 0;
}

