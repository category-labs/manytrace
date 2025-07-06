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

SEC("tp_btf/sched_wakeup_new")
int BPF_PROG(handle_sched_wakeup_new, struct task_struct *p)
{
    struct thread_event *e;
    
    u32 pid = BPF_CORE_READ(p, pid);
    u32 tgid = BPF_CORE_READ(p, tgid);

    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return 0;
    }

    e->pid = pid;
    e->tgid = tgid;
    BPF_CORE_READ_STR_INTO(&e->comm, p, comm);
    
    bpf_ringbuf_submit(e, 0);
    return 0;
}

SEC("tp_btf/sched_process_exec")
int BPF_PROG(handle_sched_process_exec, struct task_struct *p, pid_t old_pid, struct linux_binprm *bprm)
{
    struct thread_event *e;
    
    if (!bprm) {
        return 0;
    }
    
    u32 pid = BPF_CORE_READ(p, pid);
    u32 tgid = BPF_CORE_READ(p, tgid);
    
    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return 0;
    }
    
    e->pid = pid;
    e->tgid = tgid;
    
    bpf_probe_read_kernel_str(&e->filename, sizeof(e->filename), bprm->filename);
    
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