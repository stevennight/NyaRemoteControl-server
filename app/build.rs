//! Windows resources (icon, version information shown in the file's
//! properties) and the commit the build comes from (`NYA_GIT_HASH`).

fn main() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../common/assets/client.ico")
        .set("ProductName", "NyaRemoteControl")
        .set("FileDescription", "NyaRemoteControl 远程桌面")
        .set("CompanyName", "NyaRemoteControl")
        .set("LegalCopyright", "MIT License")
        .set("OriginalFilename", "NyaRemoteControl.exe");
    res.compile().expect("Windows resources (needs rc.exe from the Windows SDK)");
    println!("cargo:rerun-if-changed=../../common/assets/client.ico");
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
