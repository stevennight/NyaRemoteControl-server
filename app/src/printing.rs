//! Print jobs from the host (FEATURE_PRINT): a PDF saved in
//! `Downloads\NyaRemoteControl\打印`, then printed, opened or kept as the
//! settings say.

use std::path::Path;

fn open(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("explorer.exe").arg(path).spawn().map(|_| ())
}

/// Handle one job; returns what to tell the user. Blocking (printing).
pub fn handle(path: &Path, mode: &str) -> String {
    let name = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match mode {
        "save" => format!("被控端打印的文件已保存到 {}", path.display()),
        "open" => match open(path) {
            Ok(()) => format!("已打开被控端打印的文件（{name}）"),
            Err(e) => format!("被控端打印的文件已保存到 {}，但打不开：{e}", path.display()),
        },
        _ => {
            nya_win::com_init();
            match nya_win::print::print_pdf(path, &name, None) {
                Ok((printer, pages)) => {
                    tracing::info!("printed {} on {printer} ({pages} pages)", path.display());
                    format!("被控端的打印内容已发到打印机 {printer}（{pages} 页）")
                }
                Err(e) => {
                    tracing::warn!("printing {}: {e:#}", path.display());
                    let _ = open(path);
                    format!("没能打印（{e:#}），已打开 PDF")
                }
            }
        }
    }
}
