#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

struct thread_event {
    u32 pid;
    u32 tgid;
    char comm[16];
    char filename[256];
};

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 256 * 1024);
} events SEC(".maps");

SEC("tp/sched/sched_wakeup_new")
int handle_sched_wakeup_new(struct trace_event_raw_sched_wakeup_template* ctx)
{
    struct thread_event *e;
    
    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 pid = pid_tgid;
    u32 tgid = pid_tgid >> 32;

    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return 0;
    }

    e->pid = ctx->pid;
    e->tgid = tgid;
    bpf_probe_read_kernel_str(&e->comm, sizeof(e->comm), ctx->comm);
    
    bpf_ringbuf_submit(e, 0);
    return 0;
}

SEC("tp/sched/sched_process_exec")
int handle_sched_process_exec(struct trace_event_raw_sched_process_exec* ctx)
{
    struct thread_event *e;
    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 pid = pid_tgid;
    u32 tgid = pid_tgid >> 32;
    
    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return 0;
    }
    
    e->pid = pid;
    e->tgid = tgid;
    unsigned int filename_offset = ctx->__data_loc_filename & 0xFFFF;
    bpf_probe_read_kernel_str(&e->filename, sizeof(e->filename), 
                              (void *)ctx + filename_offset);
    
    bpf_ringbuf_submit(e, 0);
    return 0;
}

#define PR_SET_NAME 15

SEC("tp/syscalls/sys_enter_prctl")
int handle_prctl_setname(struct trace_event_raw_sys_enter* ctx)
{
    if (ctx->args[0] != PR_SET_NAME) {
        return 0;
    }
    
    struct thread_event *e;
    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 pid = pid_tgid;
    u32 tgid = pid_tgid >> 32;
    
    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return 0;
    }
    
    e->pid = pid;
    e->tgid = tgid;
    bpf_probe_read_user_str(&e->comm, sizeof(e->comm), (void *)ctx->args[1]);
    
    bpf_ringbuf_submit(e, 0);
    return 0;
}