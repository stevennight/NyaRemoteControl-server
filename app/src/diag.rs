//! `nya-client diag`: GPUs, hardware decoders, audio output.

use nya_win::d3d::D3dDevice;
use nya_win::topology::Topology;

macro_rules! out {
    ($o:expr, $($a:tt)*) => {{
        $o.push_str(&format!($($a)*));
        $o.push('\n');
    }};
}

/// `nya-client diag` in a terminal.
pub fn run() -> anyhow::Result<()> {
    print!("{}", report()?);
    Ok(())
}

/// The diagnostics report (also shown on the launcher's “关于与诊断” page).
pub fn report() -> anyhow::Result<String> {
    let mut o = String::new();
    nya_win::com_init();
    out!(o, "== NyaRemoteControl 客户端诊断 ==");
    out!(o, "版本 {} / 协议 {}.{}", crate::version(), nya_proto::PROTO_MAJOR, nya_proto::PROTO_MINOR);
    let (c, u) = nya_media::ffmpeg_versions();
    out!(o, "FFmpeg avcodec {c} / avutil {u}");
    let topo = Topology::enumerate()?;
    for a in &topo.adapters {
        out!(o, "\n[{}] {} vendor={:04x}{}", a.index, a.name, a.vendor_id, if a.software { " (软件)" } else { "" });
        for d in topo.outputs.iter().filter(|d| d.adapter_index == a.index) {
            out!(o, "    显示器 {} {}x{} {}Hz", d.device_name, d.width(), d.height(), d.refresh_hz);
        }
        match D3dDevice::for_adapter(&a.adapter) {
            Ok(d) => {
                let hw = crate::caps::hardware_decoders(&d);
                out!(o, "    硬件解码：{hw:?}");
            }
            Err(e) => out!(o, "    !! D3D11 设备：{e:#}"),
        }
    }
    match nya_win::audio::AudioRenderer::new() {
        Ok(_) => out!(o, "\n音频输出：OK"),
        Err(e) => out!(o, "\n音频输出：!! {e:#}"),
    }
    Ok(o)
}

/// One line about hardware decoding on this computer, for the launcher.
pub fn decode_summary() -> String {
    nya_win::com_init();
    let Ok(topo) = Topology::enumerate() else { return "硬件解码：未知".into() };
    let mut names = Vec::new();
    for a in topo.hardware_adapters() {
        if let Ok(d) = D3dDevice::for_adapter(&a.adapter) {
            for (codec, chroma) in crate::caps::hardware_decoders(&d) {
                let n = format!(
                    "{} {}",
                    codec.as_str_name().trim_start_matches("CODEC_").replace("H264", "H.264"),
                    if chroma == nya_proto::pb::Chroma::Yuv444 { "4:4:4" } else { "4:2:0" }
                );
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
    }
    if names.is_empty() {
        "硬件解码：不可用（软件解码）".into()
    } else {
        format!("硬件解码：{}", names.join(" / "))
    }
}
