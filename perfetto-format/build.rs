use std::io::Result;

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=protos/perfetto_trace.proto");
    prost_build::compile_protos(&["protos/perfetto_trace.proto"], &["protos/"])?;
    Ok(())
}
