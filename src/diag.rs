//! `nya-server diag`: everything needed to debug a target machine remotely.
//! Run it on the real machine and send back the output file.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use nya_media::encoder::{EncoderConfig, VideoEncoder};
use nya_win::d3d::{tex_desc, D3dDevice};
use nya_win::desktop::DesktopTracker;
use nya_win::duplication::Duplicator;
use nya_win::topology::Topology;
use nya_win::transfer::CrossGpuCopy;
use windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

use crate::host::select;

macro_rules! out {
    ($s:expr, $($arg:tt)*) => {{
        let line = format!($($arg)*);
        println!("{line}");
        let _ = writeln!($s, "{line}");
    }};
}

pub fn run(path: Option<PathBuf>) -> Result<()> {
    nya_win::com_init();
    let mut r = String::new();
    out!(r, "== NyaRemoteControl 诊断 ==");
    out!(r, "版本 {} / 协议 {}.{}", env!("CARGO_PKG_VERSION"), nya_proto::PROTO_MAJOR, nya_proto::PROTO_MINOR);
    let (c, u) = nya_media::ffmpeg_versions();
    out!(r, "FFmpeg avcodec {c} / avutil {u}  {}", match nya_media::check_runtime_versions() {
        Ok(()) => "OK".to_string(),
        Err(e) => format!("!! {e}"),
    });
    out!(r, "管理员：{}  控制台会话：{:?}", crate::winutil::is_elevated(), crate::winutil::active_console_session());

    let mut desktop = DesktopTracker::new();
    match desktop.sync() {
        Ok(_) => out!(r, "输入桌面：{}", desktop.name()),
        Err(e) => out!(r, "输入桌面：!! {e:#}"),
    }

    let topo = match Topology::enumerate() {
        Ok(t) => t,
        Err(e) => {
            out!(r, "!! 枚举显卡失败：{e:#}");
            return finish(r, path);
        }
    };
    out!(r, "\n-- 显卡 --");
    for a in &topo.adapters {
        out!(
            r,
            "[{}] {} vendor={:04x} device={:04x} 显存={}MB luid={:#x}{}",
            a.index,
            a.name,
            a.vendor_id,
            a.device_id,
            a.dedicated_video_memory >> 20,
            a.luid,
            if a.software { " (软件)" } else { "" }
        );
    }
    out!(r, "\n-- 显示器 --");
    for o in &topo.outputs {
        out!(
            r,
            "id={} {} {}x{} @({},{}) {}Hz 主显示器={} 所在显卡=[{}] 旋转={}",
            o.id,
            o.device_name,
            o.width(),
            o.height(),
            o.left,
            o.top,
            o.refresh_hz,
            o.primary,
            o.adapter_index,
            o.rotation
        );
    }

    out!(r, "\n-- 纹理格式支持（渲染目标）--");
    for a in topo.hardware_adapters() {
        match D3dDevice::for_adapter(&a.adapter) {
            Ok(dev) => out!(r, "[{}] {}", a.index, format_support(&dev)),
            Err(e) => out!(r, "[{}] !! {e:#}", a.index),
        }
    }

    out!(r, "\n-- 编码器探测（1280x720）--");
    let probes = select::probe_all(&topo);
    for p in &probes {
        out!(r, "[{}] {:?}: {:?}", p.adapter_index, p.backend, p.caps);
    }
    if probes.is_empty() {
        out!(r, "没有可用的硬件编码器，将使用软件编码（OpenH264）");
    }

    out!(r, "\n-- 编码耗时（1920x1080，30 帧平均）--");
    for p in &probes {
        let Some(a) = topo.adapter(p.adapter_index) else { continue };
        let Ok(dev) = D3dDevice::for_adapter(&a.adapter) else { continue };
        for &(codec, yuv444) in &p.caps {
            let cfg = EncoderConfig {
                backend: p.backend,
                codec,
                yuv444,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_kbps: 15_000,
                game_mode: false,
            };
            match encode_benchmark(&dev, &cfg) {
                Ok((ms, input)) => {
                    out!(r, "[{}] {:?} {:?} 444={yuv444}: {ms:.2} ms/帧（输入 {input:?}）", p.adapter_index, p.backend, codec)
                }
                Err(e) => out!(r, "[{}] {:?} {:?} 444={yuv444}: !! {e:#}", p.adapter_index, p.backend, codec),
            }
        }
    }

    out!(r, "\n-- 桌面复制（截屏）--");
    for o in &topo.outputs {
        let res = (|| -> Result<String> {
            let a = topo.adapter(o.adapter_index).ok_or_else(|| anyhow::anyhow!("adapter missing"))?;
            let dev = D3dDevice::for_adapter(&a.adapter)?;
            let mut dup = Duplicator::new(&dev, &o.output)?;
            let t = Instant::now();
            for _ in 0..10 {
                if let Some(f) = dup.acquire(100)? {
                    let got = f.image.is_some();
                    dup.release();
                    if got {
                        return Ok(format!("OK {}x{}，首帧 {} ms", dup.width, dup.height, t.elapsed().as_millis()));
                    }
                }
            }
            Ok(format!("OK {}x{}（1 秒内画面无变化，这是正常的）", dup.width, dup.height))
        })();
        match res {
            Ok(s) => out!(r, "{} {}", o.device_name, s),
            Err(e) => out!(r, "{} !! {e:#}", o.device_name),
        }
    }

    let hw: Vec<_> = topo.hardware_adapters().collect();
    if hw.len() > 1 {
        out!(r, "\n-- 跨显卡传输（NV12，内存中转）--");
        for src in &hw {
            for dst in &hw {
                if src.index == dst.index {
                    continue;
                }
                for (w, h) in [(1920u32, 1080u32), (2560, 1440), (3840, 2160)] {
                    match cross_benchmark(&src.adapter, &dst.adapter, w, h) {
                        Ok(ms) => out!(r, "[{}]→[{}] {w}x{h}: {ms:.2} ms/帧", src.index, dst.index),
                        Err(e) => out!(r, "[{}]→[{}] {w}x{h}: !! {e:#}", src.index, dst.index),
                    }
                }
            }
        }
    }

    out!(r, "\n-- 音频 --");
    match nya_win::audio::LoopbackCapture::new() {
        Ok(_) => out!(r, "系统声音回采：OK"),
        Err(e) => out!(r, "系统声音回采：!! {e:#}"),
    }

    out!(r, "\n-- 系统设置 --");
    let sas = Command("reg", &["query", r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System", "/v", "SoftwareSASGeneration"]);
    out!(r, "SoftwareSASGeneration：{}", if sas.contains("0x1") || sas.contains("0x3") { "已启用" } else { "未启用（Ctrl+Alt+Del 不可用，install 会设置）" });
    let svc = Command("sc", &["query", crate::service::SERVICE_NAME]);
    out!(r, "服务状态：{}", if svc.contains("RUNNING") { "运行中" } else if svc.contains("STOPPED") { "已停止" } else { "未安装" });

    finish(r, path)
}

#[allow(non_snake_case)]
fn Command(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn format_support(dev: &D3dDevice) -> String {
    use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_AYUV, DXGI_SAMPLE_DESC};
    const RT: u32 = 0x4000; // D3D11_FORMAT_SUPPORT_RENDER_TARGET
    let mut parts = Vec::new();
    for (name, fmt) in [("NV12", DXGI_FORMAT_NV12), ("AYUV", DXGI_FORMAT_AYUV)] {
        let flags = unsafe { dev.device.CheckFormatSupport(fmt) }.unwrap_or(0);
        let try_create = |array: u32| {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: 1920,
                Height: 1080,
                MipLevels: 1,
                ArraySize: array,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            match dev.texture(&desc) {
                Ok(_) => "OK".to_string(),
                Err(e) => format!("失败({e})"),
            }
        };
        parts.push(format!(
            "{name}: 渲染目标={} 单张={} 数组={}",
            if flags & RT != 0 { "支持" } else { "不支持" },
            try_create(1),
            try_create(6)
        ));
    }
    parts.join("  ")
}

fn encode_benchmark(dev: &D3dDevice, cfg: &EncoderConfig) -> Result<(f64, nya_media::encoder::InputFormat)> {
    let mut enc = VideoEncoder::open(cfg, dev.device_raw_owned())?;
    let input = enc.input_format();
    let mut packets = Vec::new();
    let mut total = 0.0;
    for i in 0..35 {
        let surf = enc.surface()?;
        let t = Instant::now();
        enc.encode(surf, i == 0, &mut packets)?;
        if i >= 5 {
            total += t.elapsed().as_secs_f64() * 1000.0;
        }
        packets.clear();
    }
    Ok((total / 30.0, input))
}

fn cross_benchmark(
    src: &windows::Win32::Graphics::Dxgi::IDXGIAdapter1,
    dst: &windows::Win32::Graphics::Dxgi::IDXGIAdapter1,
    w: u32,
    h: u32,
) -> Result<f64> {
    let s = D3dDevice::for_adapter(src)?;
    let d = D3dDevice::for_adapter(dst)?;
    let src_tex = s.texture(&tex_desc(w, h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET))?;
    let dst_tex = d.texture(&tex_desc(w, h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET))?;
    let mut x = CrossGpuCopy::new(&s, &d, DXGI_FORMAT_NV12, w, h)?;
    x.copy(&src_tex, &dst_tex, 0)?;
    let t = Instant::now();
    for _ in 0..20 {
        x.copy(&src_tex, &dst_tex, 0)?;
    }
    Ok(t.elapsed().as_secs_f64() * 1000.0 / 20.0)
}

fn finish(r: String, path: Option<PathBuf>) -> Result<()> {
    let path = path.unwrap_or_else(|| PathBuf::from("nya-diag.txt"));
    std::fs::write(&path, r)?;
    println!("\n结果已保存到 {}", path.display());
    Ok(())
}
