#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

enum sched_event_type {
    SCHED_EVENT_RUNNING,
    SCHED_EVENT_BLOCKED,
    SCHED_EVENT_WAKING,
};

struct sched_span_event {
    u32 pid;
    u32 tid;
    u64 start_time;
    u64 end_time;
    u32 cpu;
    u64 frame;
    u32 event_type;
};


struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 2 * 1024 * 1024);
} events SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, u32);
    __type(value, u32);
} tracked_tgids SEC(".maps");

struct task_state {
    u64 running_timestamp;
    u64 off_cpu_timestamp;
    u64 waking_timestamp;
    u32 tgid;
    u64 frame;
};

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 10000);
    __type(key, u32);
    __type(value, struct task_state);
} task_states SEC(".maps");

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


static __always_inline void handle_task_off_cpu(struct task_struct *task, u64 now, u64 *ctx)
{
    if (!task || task->pid == 0) {
        return;
    }
    
    u32 tid = BPF_CORE_READ(task, pid);
    u32 tgid = BPF_CORE_READ(task, tgid);
    
    if (!should_track_tgid(tgid)) {
        return;
    }
    
    struct task_state *state = bpf_map_lookup_elem(&task_states, &tid);
    if (state && state->running_timestamp > 0 && state->running_timestamp < now) {
        struct sched_span_event *e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
        if (e) {
            e->pid = tgid;
            e->tid = tid;
            e->start_time = state->running_timestamp;
            e->end_time = now;
            e->cpu = bpf_get_smp_processor_id();
            e->frame = 0;
            e->event_type = SCHED_EVENT_RUNNING;
            
            bpf_ringbuf_submit(e, 0);
        }
        
        state->running_timestamp = 0;
        state->off_cpu_timestamp = now;
        state->waking_timestamp = 0;
        
        u64 single_frame = 0;
        int ret = bpf_get_stack(ctx, &single_frame, sizeof(single_frame), 8 & BPF_F_SKIP_FIELD_MASK);
        if (ret == sizeof(single_frame)) {
            state->frame = single_frame;
        } else {
            state->frame = 0;
        }
    } else {
        struct task_state new_state = {
            .running_timestamp = 0,
            .off_cpu_timestamp = now,
            .waking_timestamp = 0,
            .tgid = tgid,
            .frame = 0
        };
        
        u64 single_frame = 0;
        int ret = bpf_get_stack(ctx, &single_frame, sizeof(single_frame), 8 & BPF_F_SKIP_FIELD_MASK);
        if (ret == sizeof(single_frame)) {
            new_state.frame = single_frame;
        }
        
        bpf_map_update_elem(&task_states, &tid, &new_state, BPF_ANY);
    }
}

static __always_inline void handle_task_on_cpu(struct task_struct *task, u64 now)
{
    if (!task || task->pid == 0) {
        return;
    }
    
    u32 tid = BPF_CORE_READ(task, pid);
    u32 tgid = BPF_CORE_READ(task, tgid);
    
    if (!should_track_tgid(tgid)) {
        return;
    }
    
    struct task_state *state = bpf_map_lookup_elem(&task_states, &tid);
    if (state) {
        if (state->waking_timestamp > 0 && state->waking_timestamp < now) {
            struct sched_span_event *e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
            if (e) {
                e->pid = tgid;
                e->tid = tid;
                e->start_time = state->waking_timestamp;
                e->end_time = now;
                e->cpu = bpf_get_smp_processor_id();
                e->frame = 0;
                e->event_type = SCHED_EVENT_WAKING;
                
                bpf_ringbuf_submit(e, 0);
            }
        } else if (state->off_cpu_timestamp > 0 && state->off_cpu_timestamp < now) {
            struct sched_span_event *e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
            if (e) {
                e->pid = tgid;
                e->tid = tid;
                e->start_time = state->off_cpu_timestamp;
                e->end_time = now;
                e->cpu = bpf_get_smp_processor_id();
                e->frame = state->frame;
                e->event_type = SCHED_EVENT_BLOCKED;
                
                bpf_ringbuf_submit(e, 0);
            }
        }
        
        state->running_timestamp = now;
        state->off_cpu_timestamp = 0;
        state->waking_timestamp = 0;
        state->frame = 0;
    } else {
        struct task_state new_state = {
            .running_timestamp = now,
            .off_cpu_timestamp = 0,
            .waking_timestamp = 0,
            .tgid = tgid,
            .frame = 0
        };
        bpf_map_update_elem(&task_states, &tid, &new_state, BPF_ANY);
    }
}

SEC("tp_btf/sched_switch")
int handle_sched_switch(u64 *ctx)
{
    struct task_struct *prev = (struct task_struct *)ctx[1];
    struct task_struct *next = (struct task_struct *)ctx[2];
    u64 now = bpf_ktime_get_ns();
    
    handle_task_off_cpu(prev, now, ctx);
    handle_task_on_cpu(next, now);
    
    return 0;
}

SEC("tp_btf/sched_waking")
int handle_sched_waking(u64 *ctx)
{
    struct task_struct *task = (struct task_struct *)ctx[0];
    if (!task || task->pid == 0) {
        return 0;
    }
    
    u32 tid = BPF_CORE_READ(task, pid);
    u32 tgid = BPF_CORE_READ(task, tgid);
    
    if (!should_track_tgid(tgid)) {
        return 0;
    }
    
    u64 now = bpf_ktime_get_ns();
    
    struct task_state *state = bpf_map_lookup_elem(&task_states, &tid);
    if (state && state->off_cpu_timestamp > 0 && state->off_cpu_timestamp < now) {
        struct sched_span_event *e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
        if (e) {
            e->pid = tgid;
            e->tid = tid;
            e->start_time = state->off_cpu_timestamp;
            e->end_time = now;
            e->cpu = bpf_get_smp_processor_id();
            e->frame = state->frame;
            e->event_type = SCHED_EVENT_BLOCKED;
            
            bpf_ringbuf_submit(e, 0);
        }
        
        state->off_cpu_timestamp = 0;
        state->waking_timestamp = now;
    }
    
    return 0;
}

