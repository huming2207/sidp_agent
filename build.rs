//! Generates the job and management types from proto/*.proto.
//!
//! protox compiles the schemas in pure Rust, so no protoc install is needed.
fn main() {
    println!("cargo:rerun-if-changed=proto/job.proto");
    println!("cargo:rerun-if-changed=proto/manage.proto");
    let descriptors =
        protox::compile(["job.proto", "manage.proto"], ["proto"]).expect("compile proto/*.proto");
    prost_build::Config::new()
        .compile_fds(descriptors)
        .expect("generate protobuf types");
}
