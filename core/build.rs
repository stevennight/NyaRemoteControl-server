fn main() {
    println!("cargo:rerun-if-changed=proto");
    let fds = protox::compile(["control.proto"], ["proto"]).expect("compile control.proto");
    prost_build::Config::new().compile_fds(fds).expect("generate control code");
}
