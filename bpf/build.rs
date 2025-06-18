use std::env;
use std::ffi::OsStr;
use std::path::PathBuf;

use libbpf_cargo::SkeletonBuilder;

fn build_bpf(name: &str) {
    let src = format!("src/bpf/{}.bpf.c", name);
    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR must be set in build script");
    let out = PathBuf::from(out_dir).join(format!("{}.skel.rs", name));

    let arch = env::var("CARGO_CFG_TARGET_ARCH")
        .expect("CARGO_CFG_TARGET_ARCH must be set in build script");

    SkeletonBuilder::new()
        .source(&src)
        .clang_args([
            OsStr::new("-I"),
            vmlinux::include_path_root().join(arch).as_os_str(),
        ])
        .build_and_generate(&out)
        .unwrap();

    println!("cargo:rerun-if-changed={}", src);
}

fn main() {
    build_bpf("threadtrack");
    build_bpf("cpuutil");
    build_bpf("profiler");
}
