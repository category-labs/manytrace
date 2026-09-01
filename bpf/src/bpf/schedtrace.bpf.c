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
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

enum sched_event_type {
    SCHED_EVENT_RUNNING,
    SCHED_EVENT_BLOCKED,
    SCHED_EVENT_RUNNABLE,
};

enum tracked_task_state {
    TRACKED_TASK_UNKNOWN,
    TRACKED_TASK_RUNNING,
    TRACKED_TASK_BLOCKED,
    TRACKED_TASK_RUNNABLE,
};

enum runnable_reason {
    RUNNABLE_REASON_NONE,
    RUNNABLE_REASON_PREEMPTED,
    RUNNABLE_REASON_STILL_RUNNING,
    RUNNABLE_REASON_WAKEUP,
};

struct sched_span_event {
    u32 pid;
    u32 tid;
    u64 start_time;
    u64 end_time;
    u32 cpu;
    u32 event_type;
    u64 frame;
    u32 reason;
    u32 task_state;
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
    u64 state_timestamp;
    u64 frame;
    u32 tgid;
    u32 cpu;
    u32 state;
    u32 reason;
    u32 task_state;
    u32 exiting;
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

static __always_inline bool should_track_tgid(u32 tgid)
{
    if (!cfg.filter_enabled) {
        return true;
    }
    if (tgid == 0) {
        return false;
    }
    u32 *val = bpf_map_lookup_elem(&tracked_tgids, &tgid);
    return val != NULL;
}

static __always_inline void emit_span(
    u32 tgid,
    u32 tid,
    u64 start_time,
    u64 end_time,
    u32 cpu,
    u32 event_type,
    u64 frame,
    u32 reason,
    u32 task_state)
{
    if (start_time == 0 || start_time >= end_time) {
        return;
    }

    struct sched_span_event *event =
        bpf_ringbuf_reserve(&events, sizeof(*event), 0);
    if (!event) {
        return;
    }

    event->pid = tgid;
    event->tid = tid;
    event->start_time = start_time;
    event->end_time = end_time;
    event->cpu = cpu;
    event->event_type = event_type;
    event->frame = frame;
    event->reason = reason;
    event->task_state = task_state;
    bpf_ringbuf_submit(event, 0);
}

static __always_inline u64 capture_blocking_frame(u64 *ctx)
{
    u64 frame = 0;

    // Skip scheduler internals and retain the first useful blocking frame.
    int ret = bpf_get_stack(ctx, &frame, sizeof(frame), 8);
    return ret == sizeof(frame) ? frame : 0;
}

static __always_inline void handle_task_off_cpu(
    struct task_struct *task,
    u64 now,
    u64 *ctx,
    bool preempt,
    u32 prev_state)
{
    if (!task) {
        return;
    }

    u32 tid = BPF_CORE_READ(task, pid);
    u32 tgid = BPF_CORE_READ(task, tgid);
    if (tid == 0 || !should_track_tgid(tgid)) {
        return;
    }

    u32 cpu = bpf_get_smp_processor_id();
    struct task_state *state = bpf_map_lookup_elem(&task_states, &tid);

    if (state && state->state == TRACKED_TASK_RUNNING) {
        emit_span(
            tgid,
            tid,
            state->state_timestamp,
            now,
            state->cpu,
            SCHED_EVENT_RUNNING,
            0,
            RUNNABLE_REASON_NONE,
            0);
    }

    // sched_process_exit runs before the final switch. Close the running
    // interval above, then remove the state instead of creating a stale wait.
    if (state && state->exiting) {
        bpf_map_delete_elem(&task_states, &tid);
        return;
    }

    // TASK_RUNNING is zero. A preempted task, or a task that voluntarily
    // switched while still TASK_RUNNING, remains runnable rather than blocked.
    bool runnable = preempt || prev_state == 0;
    struct task_state new_state = {
        .state_timestamp = now,
        .frame = runnable ? 0 : capture_blocking_frame(ctx),
        .tgid = tgid,
        .cpu = cpu,
        .state = runnable ? TRACKED_TASK_RUNNABLE : TRACKED_TASK_BLOCKED,
        .reason = runnable
            ? (preempt ? RUNNABLE_REASON_PREEMPTED : RUNNABLE_REASON_STILL_RUNNING)
            : RUNNABLE_REASON_NONE,
        .task_state = runnable ? 0 : prev_state,
        .exiting = 0,
    };

    if (state) {
        *state = new_state;
    } else {
        bpf_map_update_elem(&task_states, &tid, &new_state, BPF_ANY);
    }
}

static __always_inline void handle_task_on_cpu(struct task_struct *task, u64 now)
{
    if (!task) {
        return;
    }

    u32 tid = BPF_CORE_READ(task, pid);
    u32 tgid = BPF_CORE_READ(task, tgid);
    if (tid == 0 || !should_track_tgid(tgid)) {
        return;
    }

    u32 cpu = bpf_get_smp_processor_id();
    struct task_state *state = bpf_map_lookup_elem(&task_states, &tid);
    if (state) {
        if (state->state == TRACKED_TASK_RUNNABLE) {
            emit_span(
                tgid,
                tid,
                state->state_timestamp,
                now,
                cpu,
                SCHED_EVENT_RUNNABLE,
                0,
                state->reason,
                0);
        } else if (state->state == TRACKED_TASK_BLOCKED) {
            // This fallback preserves blocked time if a wakeup event was
            // missed or the task resumed without a normal wakeup tracepoint.
            emit_span(
                tgid,
                tid,
                state->state_timestamp,
                now,
                state->cpu,
                SCHED_EVENT_BLOCKED,
                state->frame,
                RUNNABLE_REASON_NONE,
                state->task_state);
        }

        state->state_timestamp = now;
        state->frame = 0;
        state->tgid = tgid;
        state->cpu = cpu;
        state->state = TRACKED_TASK_RUNNING;
        state->reason = RUNNABLE_REASON_NONE;
        state->task_state = 0;
        state->exiting = 0;
        return;
    }

    struct task_state new_state = {
        .state_timestamp = now,
        .frame = 0,
        .tgid = tgid,
        .cpu = cpu,
        .state = TRACKED_TASK_RUNNING,
        .reason = RUNNABLE_REASON_NONE,
        .task_state = 0,
        .exiting = 0,
    };
    bpf_map_update_elem(&task_states, &tid, &new_state, BPF_ANY);
}

static __always_inline void handle_task_wakeup(struct task_struct *task, u64 now)
{
    if (!task) {
        return;
    }

    u32 tid = BPF_CORE_READ(task, pid);
    u32 tgid = BPF_CORE_READ(task, tgid);
    if (tid == 0 || !should_track_tgid(tgid)) {
        return;
    }

    struct task_state *state = bpf_map_lookup_elem(&task_states, &tid);
    if (state) {
        if (state->state != TRACKED_TASK_BLOCKED) {
            return;
        }

        emit_span(
            tgid,
            tid,
            state->state_timestamp,
            now,
            state->cpu,
            SCHED_EVENT_BLOCKED,
            state->frame,
            RUNNABLE_REASON_NONE,
            state->task_state);

        state->state_timestamp = now;
        state->frame = 0;
        state->state = TRACKED_TASK_RUNNABLE;
        state->reason = RUNNABLE_REASON_WAKEUP;
        state->task_state = 0;
        return;
    }

    // A task may be woken before it has appeared in sched_switch while the
    // tracer is active. Start its runnable interval at the first known point.
    struct task_state new_state = {
        .state_timestamp = now,
        .frame = 0,
        .tgid = tgid,
        .cpu = bpf_get_smp_processor_id(),
        .state = TRACKED_TASK_RUNNABLE,
        .reason = RUNNABLE_REASON_WAKEUP,
        .task_state = 0,
        .exiting = 0,
    };
    bpf_map_update_elem(&task_states, &tid, &new_state, BPF_ANY);
}

SEC("tp_btf/sched_switch")
int handle_sched_switch(u64 *ctx)
{
    bool preempt = (bool)ctx[0];
    struct task_struct *prev = (struct task_struct *)ctx[1];
    struct task_struct *next = (struct task_struct *)ctx[2];
    u32 prev_state = (u32)ctx[3];
    u64 now = bpf_ktime_get_ns();

    handle_task_off_cpu(prev, now, ctx, preempt, prev_state);
    handle_task_on_cpu(next, now);
    return 0;
}

SEC("tp_btf/sched_wakeup")
int handle_sched_wakeup(u64 *ctx)
{
    handle_task_wakeup((struct task_struct *)ctx[0], bpf_ktime_get_ns());
    return 0;
}

SEC("tp_btf/sched_wakeup_new")
int handle_sched_wakeup_new(u64 *ctx)
{
    handle_task_wakeup((struct task_struct *)ctx[0], bpf_ktime_get_ns());
    return 0;
}

SEC("tp_btf/sched_process_exit")
int handle_sched_process_exit(u64 *ctx)
{
    struct task_struct *task = (struct task_struct *)ctx[0];
    if (!task) {
        return 0;
    }

    u32 tid = BPF_CORE_READ(task, pid);
    u32 tgid = BPF_CORE_READ(task, tgid);
    if (tid == 0 || !should_track_tgid(tgid)) {
        return 0;
    }

    struct task_state *state = bpf_map_lookup_elem(&task_states, &tid);
    if (state) {
        state->exiting = 1;
        return 0;
    }

    // Mark an unseen exiting task so its final sched_switch deletes the entry.
    struct task_state new_state = {
        .state_timestamp = bpf_ktime_get_ns(),
        .frame = 0,
        .tgid = tgid,
        .cpu = bpf_get_smp_processor_id(),
        .state = TRACKED_TASK_RUNNING,
        .reason = RUNNABLE_REASON_NONE,
        .task_state = 0,
        .exiting = 1,
    };
    bpf_map_update_elem(&task_states, &tid, &new_state, BPF_ANY);
    return 0;
}
