//! Generates the programming-job types from proto/job.proto.
//!
//! protox compiles the schema in pure Rust, so no protoc install is needed.
fn main() {
    println!("cargo:rerun-if-changed=proto/job.proto");
    let descriptors = protox::compile(["job.proto"], ["proto"]).expect("compile proto/job.proto");
    prost_build::Config::new()
        .compile_fds(descriptors)
        .expect("generate job types");
}
