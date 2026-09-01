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
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_endian.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

char LICENSE[] SEC("license") = "GPL";

#define AF_INET 2
#define AF_INET6 10
#define EAGAIN 11
#define MSG_TRUNC 0x20

enum nettrack_protocol {
    NET_PROTOCOL_TCP = 0,
    NET_PROTOCOL_UDP = 1,
};

struct net_stats {
    u64 send_bytes;
    u64 recv_bytes;
    u64 send_operations;
    u64 recv_operations;
    u64 send_errors;
    u64 recv_errors;
    u64 send_would_block;
    u64 recv_would_block;
    u64 tcp_recv_eof;
    u64 udp_recv_truncated;
    u64 first_seen_ns;
    u64 last_seen_ns;
};

struct process_key {
    u32 tgid;
    u8 protocol;
    u8 padding[3];
};

struct peer_key {
    u32 tgid;
    u8 protocol;
    u8 family;
    u16 local_port;
    u16 remote_port;
    u16 padding;
    u8 remote_addr[16];
};

struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 2);
    __type(key, u32);
    __type(value, struct net_stats);
} global_stats SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 1024);
    __type(key, struct process_key);
    __type(value, struct net_stats);
} process_stats SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 4096);
    __type(key, struct peer_key);
    __type(value, struct net_stats);
} peer_stats SEC(".maps");

struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 1024);
    __type(key, u32);
    __type(value, u32);
} tracked_tgids SEC(".maps");

const volatile struct {
    bool tcp_enabled;
    bool udp_enabled;
    bool send_enabled;
    bool receive_enabled;
    bool host_enabled;
    bool process_enabled;
    bool peer_enabled;
    bool bytes_enabled;
    bool operations_enabled;
    bool errors_enabled;
    bool filter_enabled;
} cfg = {
    .tcp_enabled = true,
    .udp_enabled = true,
    .send_enabled = true,
    .receive_enabled = true,
    .host_enabled = true,
    .process_enabled = false,
    .peer_enabled = false,
    .bytes_enabled = true,
    .operations_enabled = false,
    .errors_enabled = false,
    .filter_enabled = false,
};

static __always_inline bool should_track_tgid(u32 tgid)
{
    if (tgid == 0)
        return false;
    if (!cfg.filter_enabled)
        return true;
    return bpf_map_lookup_elem(&tracked_tgids, &tgid) != NULL;
}

static __always_inline void initialize_stats(struct net_stats *stats, u64 now)
{
    __builtin_memset(stats, 0, sizeof(*stats));
    stats->first_seen_ns = now;
    stats->last_seen_ns = now;
}

static __always_inline void update_stats(struct net_stats *stats, bool send, int ret,
                                         int protocol, u32 msg_flags, u64 now,
                                         bool shared)
{
    if (shared)
        __sync_lock_test_and_set(&stats->last_seen_ns, now);
    else
        stats->last_seen_ns = now;

    if (ret < 0) {
        if (!cfg.errors_enabled)
            return;
        if (send) {
            if (shared)
                __sync_fetch_and_add(&stats->send_errors, 1);
            else
                stats->send_errors++;
            if (ret == -EAGAIN) {
                if (shared)
                    __sync_fetch_and_add(&stats->send_would_block, 1);
                else
                    stats->send_would_block++;
            }
        } else {
            if (shared)
                __sync_fetch_and_add(&stats->recv_errors, 1);
            else
                stats->recv_errors++;
            if (ret == -EAGAIN) {
                if (shared)
                    __sync_fetch_and_add(&stats->recv_would_block, 1);
                else
                    stats->recv_would_block++;
            }
        }
        return;
    }

    if (send) {
        if (cfg.bytes_enabled && ret > 0) {
            if (shared)
                __sync_fetch_and_add(&stats->send_bytes, (u64)ret);
            else
                stats->send_bytes += (u64)ret;
        }
        if (cfg.operations_enabled) {
            if (shared)
                __sync_fetch_and_add(&stats->send_operations, 1);
            else
                stats->send_operations++;
        }
    } else {
        if (cfg.bytes_enabled && ret > 0) {
            if (shared)
                __sync_fetch_and_add(&stats->recv_bytes, (u64)ret);
            else
                stats->recv_bytes += (u64)ret;
        }
        if (cfg.operations_enabled) {
            if (shared)
                __sync_fetch_and_add(&stats->recv_operations, 1);
            else
                stats->recv_operations++;

            if (protocol == NET_PROTOCOL_TCP && ret == 0) {
                if (shared)
                    __sync_fetch_and_add(&stats->tcp_recv_eof, 1);
                else
                    stats->tcp_recv_eof++;
            }
            if (protocol == NET_PROTOCOL_UDP && (msg_flags & MSG_TRUNC)) {
                if (shared)
                    __sync_fetch_and_add(&stats->udp_recv_truncated, 1);
                else
                    stats->udp_recv_truncated++;
            }
        }
    }
}

static __always_inline void update_process_stats(u32 tgid, int protocol, bool send,
                                                 int ret, u32 msg_flags, u64 now)
{
    struct process_key key = {
        .tgid = tgid,
        .protocol = protocol,
    };
    struct net_stats *stats = bpf_map_lookup_elem(&process_stats, &key);
    if (stats) {
        update_stats(stats, send, ret, protocol, msg_flags, now, true);
        return;
    }

    struct net_stats initial;
    initialize_stats(&initial, now);
    update_stats(&initial, send, ret, protocol, msg_flags, now, false);
    if (bpf_map_update_elem(&process_stats, &key, &initial, BPF_NOEXIST) == 0)
        return;

    stats = bpf_map_lookup_elem(&process_stats, &key);
    if (stats)
        update_stats(stats, send, ret, protocol, msg_flags, now, true);
}

static __always_inline bool peer_from_sock(struct sock *sk, struct peer_key *key)
{
    u16 family = BPF_CORE_READ(sk, __sk_common.skc_family);
    key->family = family;
    key->local_port = BPF_CORE_READ(sk, __sk_common.skc_num);
    key->remote_port = bpf_ntohs(BPF_CORE_READ(sk, __sk_common.skc_dport));

    if (family == AF_INET) {
        u32 address = BPF_CORE_READ(sk, __sk_common.skc_daddr);
        __builtin_memcpy(key->remote_addr, &address, sizeof(address));
        return address != 0 && key->remote_port != 0;
    }
    if (family == AF_INET6) {
        BPF_CORE_READ_INTO(key->remote_addr, sk, __sk_common.skc_v6_daddr);
        return key->remote_port != 0;
    }
    return false;
}

static __always_inline bool peer_from_message(struct msghdr *msg, struct peer_key *key)
{
    void *name;
    u16 family;

    if (!msg)
        return false;
    name = BPF_CORE_READ(msg, msg_name);
    if (!name)
        return false;
    if (bpf_probe_read_kernel(&family, sizeof(family), name) != 0)
        return false;

    if (family == AF_INET) {
        struct sockaddr_in address;
        if (bpf_probe_read_kernel(&address, sizeof(address), name) != 0)
            return false;
        key->family = AF_INET;
        key->remote_port = bpf_ntohs(address.sin_port);
        __builtin_memcpy(key->remote_addr, &address.sin_addr, sizeof(address.sin_addr));
        return key->remote_port != 0;
    }
    if (family == AF_INET6) {
        struct sockaddr_in6 address;
        if (bpf_probe_read_kernel(&address, sizeof(address), name) != 0)
            return false;
        key->family = AF_INET6;
        key->remote_port = bpf_ntohs(address.sin6_port);
        __builtin_memcpy(key->remote_addr, &address.sin6_addr, sizeof(address.sin6_addr));
        return key->remote_port != 0;
    }
    return false;
}

static __always_inline void update_peer_stats(struct sock *sk, struct msghdr *msg,
                                              u32 tgid, int protocol, bool send,
                                              int ret, u32 msg_flags, u64 now)
{
    struct peer_key key = {
        .tgid = tgid,
        .protocol = protocol,
    };

    key.local_port = BPF_CORE_READ(sk, __sk_common.skc_num);
    bool found = false;
    if (protocol == NET_PROTOCOL_UDP)
        found = peer_from_message(msg, &key);
    if (!found)
        found = peer_from_sock(sk, &key);
    if (!found)
        return;

    struct net_stats *stats = bpf_map_lookup_elem(&peer_stats, &key);
    if (stats) {
        update_stats(stats, send, ret, protocol, msg_flags, now, true);
        return;
    }

    struct net_stats initial;
    initialize_stats(&initial, now);
    update_stats(&initial, send, ret, protocol, msg_flags, now, false);
    if (bpf_map_update_elem(&peer_stats, &key, &initial, BPF_NOEXIST) == 0)
        return;

    stats = bpf_map_lookup_elem(&peer_stats, &key);
    if (stats)
        update_stats(stats, send, ret, protocol, msg_flags, now, true);
}

static __always_inline int account_socket_io(struct sock *sk, struct msghdr *msg,
                                             int protocol, bool send, int ret)
{
    if ((send && !cfg.send_enabled) || (!send && !cfg.receive_enabled))
        return 0;

    u64 pid_tgid = bpf_get_current_pid_tgid();
    u32 tgid = pid_tgid >> 32;
    if (!should_track_tgid(tgid))
        return 0;

    if (!sk || (protocol == NET_PROTOCOL_TCP && !cfg.tcp_enabled)
        || (protocol == NET_PROTOCOL_UDP && !cfg.udp_enabled))
        return 0;

    u64 now = bpf_ktime_get_ns();
    u32 msg_flags = msg ? BPF_CORE_READ(msg, msg_flags) : 0;

    if (cfg.host_enabled) {
        u32 key = protocol;
        struct net_stats *stats = bpf_map_lookup_elem(&global_stats, &key);
        if (stats)
            update_stats(stats, send, ret, protocol, msg_flags, now, false);
    }
    if (cfg.process_enabled)
        update_process_stats(tgid, protocol, send, ret, msg_flags, now);
    if (cfg.peer_enabled)
        update_peer_stats(sk, msg, tgid, protocol, send, ret, msg_flags, now);
    return 0;
}

SEC("fexit/tcp_sendmsg")
int BPF_PROG(trace_tcp_sendmsg, struct sock *sk, struct msghdr *msg, size_t size, int ret)
{
    return account_socket_io(sk, msg, NET_PROTOCOL_TCP, true, ret);
}

SEC("fexit/tcp_recvmsg")
int BPF_PROG(trace_tcp_recvmsg, struct sock *sk, struct msghdr *msg, size_t len,
             int flags, int *addr_len, int ret)
{
    return account_socket_io(sk, msg, NET_PROTOCOL_TCP, false, ret);
}

SEC("fexit/udp_sendmsg")
int BPF_PROG(trace_udp_sendmsg, struct sock *sk, struct msghdr *msg, size_t len, int ret)
{
    return account_socket_io(sk, msg, NET_PROTOCOL_UDP, true, ret);
}

SEC("fexit/udp_recvmsg")
int BPF_PROG(trace_udp_recvmsg, struct sock *sk, struct msghdr *msg, size_t len,
             int flags, int *addr_len, int ret)
{
    return account_socket_io(sk, msg, NET_PROTOCOL_UDP, false, ret);
}
