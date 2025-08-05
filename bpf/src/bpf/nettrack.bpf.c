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

struct net_stats {
    u64 tcp_send_bytes;
    u64 tcp_recv_bytes;
    u64 udp_send_bytes;
    u64 udp_recv_bytes;
    u64 last_update_ns;
};

struct net_event {
    u64 timestamp;
    u64 tcp_send_bytes;
    u64 tcp_recv_bytes;
    u64 udp_send_bytes;
    u64 udp_recv_bytes;
    u64 total_send_bytes;
    u64 total_recv_bytes;
    u32 cpu;
};

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, u32);
    __type(value, struct net_stats);
} net_stats SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_RINGBUF);
    __uint(max_entries, 1024 * 1024);
} events SEC(".maps");

static __always_inline u64 get_time() {
    return bpf_ktime_get_ns();
}

static __always_inline void submit_net_event(struct net_stats *stats, u64 now) {
    struct net_event *e;
    
    e = bpf_ringbuf_reserve(&events, sizeof(*e), 0);
    if (!e) {
        return;
    }
    
    e->timestamp = now;
    e->tcp_send_bytes = stats->tcp_send_bytes;
    e->tcp_recv_bytes = stats->tcp_recv_bytes;
    e->udp_send_bytes = stats->udp_send_bytes;
    e->udp_recv_bytes = stats->udp_recv_bytes;
    e->total_send_bytes = stats->tcp_send_bytes + stats->udp_send_bytes;
    e->total_recv_bytes = stats->tcp_recv_bytes + stats->udp_recv_bytes;
    e->cpu = bpf_get_smp_processor_id();
    
    bpf_ringbuf_submit(e, 0);
    
    stats->tcp_send_bytes = 0;
    stats->tcp_recv_bytes = 0;
    stats->udp_send_bytes = 0;
    stats->udp_recv_bytes = 0;
    stats->last_update_ns = now;
}

SEC("kprobe/udp_sendmsg")
int BPF_KPROBE(trace_udp_sendmsg, struct sock *sk, struct msghdr *msg, size_t size)
{
    u32 zero = 0;
    struct net_stats *stats = bpf_map_lookup_elem(&net_stats, &zero);
    if (!stats) {
        return 0;
    }
    
    stats->udp_send_bytes += size;
    
    return 0;
}

SEC("kretprobe/udp_recvmsg")
int BPF_KRETPROBE(trace_udp_recvmsg_ret, int ret)
{
    u32 zero = 0;
    struct net_stats *stats = bpf_map_lookup_elem(&net_stats, &zero);
    if (!stats) {
        return 0;
    }
    
    if (ret > 0) {
        stats->udp_recv_bytes += (u64)ret;
    }
    
    return 0;
}

SEC("kprobe/tcp_sendmsg")
int BPF_KPROBE(trace_tcp_sendmsg, struct sock *sk, struct msghdr *msg, size_t size)
{
    u32 zero = 0;
    struct net_stats *stats = bpf_map_lookup_elem(&net_stats, &zero);
    if (!stats) {
        return 0;
    }
    
    stats->tcp_send_bytes += size;
    
    return 0;
}

SEC("kretprobe/tcp_recvmsg")
int BPF_KRETPROBE(trace_tcp_recvmsg_ret, int ret)
{
    u32 zero = 0;
    struct net_stats *stats = bpf_map_lookup_elem(&net_stats, &zero);
    if (!stats) {
        return 0;
    }
    
    if (ret > 0) {
        stats->tcp_recv_bytes += (u64)ret;
    }
    
    return 0;
}

SEC("perf_event")
int handle_boundary_event(void *ctx)
{
    u32 zero = 0;
    struct net_stats *stats = bpf_map_lookup_elem(&net_stats, &zero);
    if (!stats) {
        return 0;
    }
    
    u64 now = get_time();
    if (stats->last_update_ns == 0) {
        stats->last_update_ns = now;
        return 0;
    }
    
    submit_net_event(stats, now);
    
    return 0;
}
