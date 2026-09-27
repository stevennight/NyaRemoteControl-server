//! Encoder probing and selection (design doc §3.5 "编码 GPU 选择策略").

use nya_media::encoder::{Backend, EncoderConfig, VideoEncoder};
use nya_media::VideoCodec;
use nya_proto::pb;
use nya_win::d3d::D3dDevice;
use nya_win::topology::{Topology, Vendor};

/// What one GPU's hardware encoder can do (found by actually opening it).
#[derive(Debug, Clone)]
pub struct EncoderProbe {
    pub adapter_index: u32,
    pub backend: Backend,
    /// (codec, yuv444)
    pub caps: Vec<(VideoCodec, bool)>,
}

impl EncoderProbe {
    pub fn has(&self, codec: VideoCodec, yuv444: bool) -> bool {
        self.caps.contains(&(codec, yuv444))
    }
}

pub fn backend_for(v: Vendor) -> Option<Backend> {
    match v {
        Vendor::Nvidia => Some(Backend::Nvenc),
        Vendor::Intel => Some(Backend::Qsv),
        Vendor::Amd => Some(Backend::Amf),
        _ => None,
    }
}

pub fn to_pb_codec(c: VideoCodec) -> pb::Codec {
    match c {
        VideoCodec::H264 => pb::Codec::H264,
        VideoCodec::Hevc => pb::Codec::Hevc,
        VideoCodec::Av1 => pb::Codec::Av1,
    }
}

pub fn from_pb_codec(c: i32) -> Option<VideoCodec> {
    match pb::Codec::try_from(c).ok()? {
        pb::Codec::H264 => Some(VideoCodec::H264),
        pb::Codec::Hevc => Some(VideoCodec::Hevc),
        pb::Codec::Av1 => Some(VideoCodec::Av1),
        pb::Codec::Unspecified => None,
    }
}

/// Try to open a small encoder for every codec/chroma on every hardware GPU.
pub fn probe_all(topo: &Topology) -> Vec<EncoderProbe> {
    let mut out = Vec::new();
    for a in topo.hardware_adapters() {
        let Some(backend) = backend_for(a.vendor()) else { continue };
        let dev = match D3dDevice::for_adapter(&a.adapter) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("{}: cannot create D3D11 device: {e:#}", a.name);
                continue;
            }
        };
        let mut caps = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Av1] {
            for yuv444 in [false, true] {
                if yuv444 && !backend.supports_444(codec) {
                    continue;
                }
                let cfg = EncoderConfig {
                    backend,
                    codec,
                    yuv444,
                    width: 1280,
                    height: 720,
                    fps: 60,
                    bitrate_kbps: 5000,
                    game_mode: false,
                };
                match VideoEncoder::open(&cfg, dev.device_raw_owned()) {
                    Ok(_) => caps.push((codec, yuv444)),
                    Err(e) => tracing::debug!("{} {:?} 444={yuv444}: {e:#}", a.name, codec),
                }
            }
        }
        tracing::info!("GPU {} ({}): {:?} {:?}", a.index, a.name, backend, caps);
        out.push(EncoderProbe { adapter_index: a.index, backend, caps });
    }
    out
}

/// One way to build the pipeline; tried in order until one works.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Plan {
    pub capture_adapter: u32,
    /// None = software encoder.
    pub encode_adapter: Option<u32>,
    pub backend: Backend,
    pub codec: VideoCodec,
    pub yuv444: bool,
}

/// What the client says it can decode. `None` = unknown: assume 4:2:0 H.264/HEVC in hardware.
fn client_decodes(caps: Option<&pb::ClientCaps>, codec: VideoCodec, yuv444: bool) -> Option<bool> {
    let Some(caps) = caps else {
        return (!yuv444 && codec != VideoCodec::Av1).then_some(true);
    };
    let want_chroma = if yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 } as i32;
    let mut found = None;
    for d in &caps.decoders {
        if from_pb_codec(d.codec) == Some(codec) && d.chroma == want_chroma {
            found = Some(found.unwrap_or(false) || d.hardware);
        }
    }
    found
}

pub fn plans(
    probes: &[EncoderProbe],
    capture_adapter: u32,
    req: &pb::StartStream,
    caps: Option<&pb::ClientCaps>,
    preference: &str,
) -> Vec<Plan> {
    let cfg = req.config.clone().unwrap_or_default();
    let mode_game = cfg.mode == pb::StreamMode::Game as i32;
    let pref = if req.encoder_preference.is_empty() || req.encoder_preference == "auto" {
        preference
    } else {
        req.encoder_preference.as_str()
    };
    let pref_backend = Backend::from_name(pref);

    // Codec order.
    let codecs: Vec<VideoCodec> = match from_pb_codec(cfg.codec) {
        Some(c) => vec![c],
        None => vec![VideoCodec::Hevc, VideoCodec::H264],
    }
    .into_iter()
    .filter(|&c| client_decodes(caps, c, false).is_some())
    .collect();

    // Chroma wish: explicit request, else 4:4:4 in office mode if the client decodes it in hardware.
    let want444 = match pb::Chroma::try_from(cfg.chroma).unwrap_or(pb::Chroma::Unspecified) {
        pb::Chroma::Yuv444 => true,
        pb::Chroma::Yuv420 => false,
        pb::Chroma::Unspecified => {
            !mode_game && codecs.iter().any(|&c| client_decodes(caps, c, true) == Some(true))
        }
    };

    // GPU order: the capture GPU first (no copy), then other GPUs with NVENC first.
    let mut gpus: Vec<&EncoderProbe> = probes.iter().collect();
    gpus.sort_by_key(|p| (p.adapter_index != capture_adapter, p.backend != Backend::Nvenc, p.adapter_index));
    if let Some(b) = pref_backend {
        gpus.sort_by_key(|p| p.backend != b);
    }

    let mut out = Vec::new();
    if pref_backend != Some(Backend::Software) {
        let chroma_order: &[bool] = if want444 { &[true, false] } else { &[false] };
        for &yuv444 in chroma_order {
            for p in &gpus {
                for &codec in &codecs {
                    if !p.has(codec, yuv444) {
                        continue;
                    }
                    if yuv444 && client_decodes(caps, codec, true).is_none() {
                        continue;
                    }
                    out.push(Plan {
                        capture_adapter,
                        encode_adapter: Some(p.adapter_index),
                        backend: p.backend,
                        codec,
                        yuv444,
                    });
                }
            }
        }
    }
    if client_decodes(caps, VideoCodec::H264, false).is_some() {
        out.push(Plan {
            capture_adapter,
            encode_adapter: None,
            backend: Backend::Software,
            codec: VideoCodec::H264,
            yuv444: false,
        });
    }
    out
}

/// Default bitrate (kbit/s) when neither client nor config specify one.
pub fn auto_bitrate(w: u32, h: u32, fps: u32, game: bool, yuv444: bool) -> u32 {
    let bits_per_pixel = if game { 0.12 } else { 0.06 } * if yuv444 { 1.5 } else { 1.0 };
    let kbps = (w as f64 * h as f64 * fps as f64 * bits_per_pixel / 1000.0) as u32;
    kbps.clamp(2_000, 80_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(idx: u32, backend: Backend, caps: &[(VideoCodec, bool)]) -> EncoderProbe {
        EncoderProbe { adapter_index: idx, backend, caps: caps.to_vec() }
    }

    fn hevc_caps(hw444: bool) -> pb::ClientCaps {
        let mut decoders = vec![
            pb::CodecCap { codec: pb::Codec::H264 as i32, chroma: pb::Chroma::Yuv420 as i32, hardware: true, ..Default::default() },
            pb::CodecCap { codec: pb::Codec::Hevc as i32, chroma: pb::Chroma::Yuv420 as i32, hardware: true, ..Default::default() },
        ];
        decoders.push(pb::CodecCap {
            codec: pb::Codec::Hevc as i32,
            chroma: pb::Chroma::Yuv444 as i32,
            hardware: hw444,
            ..Default::default()
        });
        pb::ClientCaps { decoders, ..Default::default() }
    }

    fn office() -> pb::StartStream {
        pb::StartStream {
            config: Some(pb::StreamConfig { mode: pb::StreamMode::Office as i32, ..Default::default() }),
            ..Default::default()
        }
    }

    use VideoCodec::*;

    /// Laptop: display on the Intel iGPU (0), NVIDIA dGPU (1).
    fn laptop(intel_444: bool) -> Vec<EncoderProbe> {
        let mut intel = vec![(H264, false), (Hevc, false)];
        if intel_444 {
            intel.push((Hevc, true));
        }
        vec![
            probe(0, Backend::Qsv, &intel),
            probe(1, Backend::Nvenc, &[(H264, false), (Hevc, false), (H264, true), (Hevc, true)]),
        ]
    }

    #[test]
    fn same_gpu_444_preferred() {
        let p = plans(&laptop(true), 0, &office(), Some(&hevc_caps(true)), "auto");
        assert_eq!(p[0], Plan { capture_adapter: 0, encode_adapter: Some(0), backend: Backend::Qsv, codec: Hevc, yuv444: true });
    }

    #[test]
    fn other_gpu_when_capture_gpu_lacks_444() {
        let p = plans(&laptop(false), 0, &office(), Some(&hevc_caps(true)), "auto");
        assert_eq!(p[0].encode_adapter, Some(1));
        assert_eq!(p[0].backend, Backend::Nvenc);
        assert!(p[0].yuv444);
        // 4:2:0 on the capture GPU is the next best option.
        assert!(p.iter().any(|x| x.encode_adapter == Some(0) && !x.yuv444));
        assert_eq!(p.last().unwrap().backend, Backend::Software);
    }

    #[test]
    fn game_mode_uses_420_on_capture_gpu() {
        let mut req = office();
        req.config.as_mut().unwrap().mode = pb::StreamMode::Game as i32;
        let p = plans(&laptop(true), 0, &req, Some(&hevc_caps(true)), "auto");
        assert_eq!(p[0], Plan { capture_adapter: 0, encode_adapter: Some(0), backend: Backend::Qsv, codec: Hevc, yuv444: false });
    }

    #[test]
    fn preference_forces_nvenc() {
        let p = plans(&laptop(true), 0, &office(), Some(&hevc_caps(true)), "nvenc");
        assert_eq!(p[0].backend, Backend::Nvenc);
    }

    #[test]
    fn no_444_when_client_only_decodes_in_software() {
        let p = plans(&laptop(true), 0, &office(), Some(&hevc_caps(false)), "auto");
        assert!(!p[0].yuv444);
    }

    #[test]
    fn software_only_when_no_gpu() {
        let p = plans(&[], 0, &office(), None, "auto");
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].backend, Backend::Software);
    }

    #[test]
    fn bitrate_bounds() {
        assert_eq!(auto_bitrate(640, 360, 30, false, false), 2_000);
        assert_eq!(auto_bitrate(7680, 4320, 144, true, true), 80_000);
        let b = auto_bitrate(1920, 1080, 60, false, false);
        assert!((5_000..12_000).contains(&b), "{b}");
    }
}
