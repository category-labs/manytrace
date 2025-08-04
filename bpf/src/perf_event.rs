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

use std::{io, mem};

use libbpf_rs::libbpf_sys::{perf_event_attr, PERF_SAMPLE_RAW};
use libc::{self, SYS_perf_event_open};

pub(crate) fn attach_perf_event(
    pefds: &[i32],
    prog: &mut libbpf_rs::ProgramMut,
) -> Result<Vec<libbpf_rs::Link>, libbpf_rs::Error> {
    pefds
        .iter()
        .map(|pefd| prog.attach_perf_event(*pefd))
        .collect()
}

pub(crate) fn perf_event_per_cpu(
    type_: u32,
    config: u32,
    freq: u64,
) -> Result<Vec<i32>, io::Error> {
    let nprocs = libbpf_rs::num_possible_cpus().unwrap();
    (0..nprocs)
        .map(|cpu| perf_event_open(type_, config, 0, Some(freq), -1, cpu as i32, 0))
        .collect()
}

pub(crate) fn perf_event_open(
    type_: u32,
    config: u32,
    period: u64,
    frequency: Option<u64>,
    pid: i32,
    cpu: i32,
    flags: u32,
) -> Result<i32, io::Error> {
    let mut attr = unsafe { mem::zeroed::<perf_event_attr>() };
    attr.config = config as u64;
    attr.size = mem::size_of::<perf_event_attr>() as u32;
    attr.type_ = type_;
    attr.sample_type = PERF_SAMPLE_RAW as u64;
    attr.set_inherit(0);
    attr.__bindgen_anon_2.wakeup_events = 0;

    if let Some(frequency) = frequency {
        attr.set_freq(1);
        attr.__bindgen_anon_1.sample_freq = frequency;
    } else {
        attr.__bindgen_anon_1.sample_period = period;
    }

    let rst = unsafe { libc::syscall(SYS_perf_event_open, &attr, pid, cpu, -1, flags) };
    match rst {
        rst @ 0.. => Ok(rst as i32),
        _ => Err(io::Error::last_os_error()),
    }
}
