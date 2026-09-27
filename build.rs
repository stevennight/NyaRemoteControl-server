fn main() {
    let common_proto = "../common/crates/nya-proto/proto";
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed={common_proto}");
    let fds = protox::compile(["ipc.proto"], ["proto", common_proto]).expect("compile ipc.proto");
    prost_build::Config::new()
        .extern_path(".nya.v1", "::nya_proto::pb")
        .compile_fds(fds)
        .expect("generate ipc code");
}
