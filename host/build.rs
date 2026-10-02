fn main() {
    let common_proto = "../../common/crates/nya-proto/proto";
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
    let vigem = std::path::Path::new("../../third_party/ViGEmClient");
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

    // Icon and version information shown in the file's properties.
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../common/assets/server.ico")
        .set("ProductName", "NyaRemoteControl")
        .set("FileDescription", "NyaRemoteControl service (remote control of this computer)")
        .set("CompanyName", "NyaRemoteControl")
        .set("LegalCopyright", "MIT License")
        .set("OriginalFilename", "nya-server-svc.exe");
    res.compile().expect("Windows resources (needs rc.exe from the Windows SDK)");
    println!("cargo:rerun-if-changed=../../common/assets/server.ico");
    git_hash();
}

/// `NYA_GIT_HASH`: short commit id, `+` when the tree has changes ("" outside git).
fn git_hash() {
    let git = |args: &[&str]| {
        std::process::Command::new("git").args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let hash = git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_default();
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=NYA_GIT_HASH={hash}{}", if dirty && !hash.is_empty() { "+" } else { "" });
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
}
