fn main() {
    let common_proto = "../common/crates/nya-proto/proto";
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-changed={common_proto}");
    let fds = protox::compile(["ipc.proto"], ["proto", common_proto]).expect("compile ipc.proto");
    prost_build::Config::new()
        .extern_path(".nya.v1", "::nya_proto::pb")
        .compile_fds(fds)
        .expect("generate ipc code");

    // Gamepads: ViGEmClient (MIT) is compiled in when its source is present
    // (common/scripts/fetch-vigem.ps1 puts it in ../third_party/ViGEmClient).
    println!("cargo:rustc-check-cfg=cfg(vigem)");
    let vigem = std::path::Path::new("../third_party/ViGEmClient");
    println!("cargo:rerun-if-changed={}", vigem.display());
    if vigem.join("src/ViGEmClient.cpp").exists() {
        cc::Build::new()
            .cpp(true)
            .file(vigem.join("src/ViGEmClient.cpp"))
            .include(vigem.join("include"))
            .define("UNICODE", None)
            .define("_UNICODE", None)
            .warnings(false)
            .compile("vigemclient");
        println!("cargo:rustc-link-lib=setupapi");
        println!("cargo:rustc-cfg=vigem");
    } else {
        println!("cargo:warning=ViGEmClient source not found; building without gamepad support");
    }
}
