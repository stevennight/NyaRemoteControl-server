//! Decode thread: header parsing, gap detection / keyframe requests, hardware
//! decode with software fallback, and upload into a small ring of
//! shader-readable textures that the renderer samples.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use crossbeam_channel::Receiver;
use nya_media::decoder::{DecodedFrame, FrameData, Matrix, PixelLayout, VideoDecoder};
use nya_media::VideoCodec;
use nya_proto::frame::VideoFrameHeader;
use nya_proto::pb::{self, control_msg::Msg};
use nya_win::d3d::{tex_desc, D3dDevice};
use tokio::sync::mpsc::UnboundedSender;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::events::{NetCmd, Ui, UiEvent};
use crate::stats::Shared;

pub enum VideoIn {
    Frame { stream_id: u64, buf: Vec<u8> },
    /// New stream / reconnect: drop decoder state and wait for a keyframe.
    Reset,
    /// The renderer moved to another GPU.
    Device(D3dDevice),
}

/// How sample values are stored, for the YUV offsets and scales.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 8-bit unorm.
    Eight,
    /// 10-bit in the top bits of 16 (P010).
    TenMsb,
    /// 10-bit in the low bits of 16 (FFmpeg's yuv420p10le).
    TenLsb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// Y (R8) + interleaved UV (R8G8), either one NV12 texture or two textures.
    Nv12,
    /// AYUV (R8G8B8A8 view: V U Y A).
    Ayuv,
    /// Three R8 planes (4:2:0 or 4:4:4).
    Planar,
}

pub struct Slot {
    pub kind: SlotKind,
    pub width: u32,
    pub height: u32,
    pub full_range: bool,
    pub matrix: Matrix,
    pub depth: Depth,
    /// HDR10 (PQ, BT.2020).
    pub pq: bool,
    pub srvs: Vec<ID3D11ShaderResourceView>,
    pub capture_ts: u64,
}

/// Latest decoded frame, picked up by the renderer.
#[derive(Default)]
pub struct FrameStore {
    latest: Mutex<Option<Arc<Slot>>>,
    fresh: Mutex<bool>,
}

impl FrameStore {
    fn put(&self, slot: Arc<Slot>) -> bool {
        *self.latest.lock().unwrap() = Some(slot);
        let mut f = self.fresh.lock().unwrap();
        let replaced_unrendered = *f;
        *f = true;
        replaced_unrendered
    }

    /// Latest frame and whether it hasn't been rendered yet.
    pub fn take(&self) -> (Option<Arc<Slot>>, bool) {
        let s = self.latest.lock().unwrap().clone();
        let mut f = self.fresh.lock().unwrap();
        let fresh = *f;
        *f = false;
        (s, fresh)
    }

    /// Kind of the latest frame, for logging.
    pub fn take_kind(&self) -> Option<SlotKind> {
        self.latest.lock().unwrap().as_ref().map(|s| s.kind)
    }

    pub fn clear(&self) {
        *self.latest.lock().unwrap() = None;
    }
}

struct RingSlot {
    textures: Vec<ID3D11Texture2D>,
    srvs: Vec<ID3D11ShaderResourceView>,
}

#[derive(Default)]
struct Ring {
    key: Option<(PixelLayout, u32, u32)>,
    slots: Vec<RingSlot>,
    next: usize,
}

fn srv(dev: &D3dDevice, tex: &ID3D11Texture2D, fmt: DXGI_FORMAT) -> Result<ID3D11ShaderResourceView> {
    let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: fmt,
        ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_SRV { MostDetailedMip: 0, MipLevels: 1 } },
    };
    let mut v = None;
    unsafe { dev.device.CreateShaderResourceView(tex, Some(&desc), Some(&mut v))? };
    v.ok_or_else(|| anyhow!("CreateShaderResourceView"))
}

impl Ring {
    fn slot(&mut self, dev: &D3dDevice, layout: PixelLayout, w: u32, h: u32) -> Result<&RingSlot> {
        if self.key != Some((layout, w, h)) {
            self.slots.clear();
            for _ in 0..3 {
                let mk = |fmt, w, h| dev.texture(&tex_desc(w, h, fmt, D3D11_BIND_SHADER_RESOURCE));
                let (textures, srvs) = match layout {
                    PixelLayout::Nv12 => {
                        let t = mk(DXGI_FORMAT_NV12, w, h)?;
                        let s = vec![srv(dev, &t, DXGI_FORMAT_R8_UNORM)?, srv(dev, &t, DXGI_FORMAT_R8G8_UNORM)?];
                        (vec![t], s)
                    }
                    PixelLayout::Ayuv => {
                        let t = mk(DXGI_FORMAT_AYUV, w, h)?;
                        let s = vec![srv(dev, &t, DXGI_FORMAT_R8G8B8A8_UNORM)?];
                        (vec![t], s)
                    }
                    PixelLayout::P010 => {
                        let t = mk(DXGI_FORMAT_P010, w, h)?;
                        let s = vec![srv(dev, &t, DXGI_FORMAT_R16_UNORM)?, srv(dev, &t, DXGI_FORMAT_R16G16_UNORM)?];
                        (vec![t], s)
                    }
                    PixelLayout::P010Cpu => {
                        let y = mk(DXGI_FORMAT_R16_UNORM, w, h)?;
                        let uv = mk(DXGI_FORMAT_R16G16_UNORM, w.div_ceil(2), h.div_ceil(2))?;
                        let s = vec![srv(dev, &y, DXGI_FORMAT_R16_UNORM)?, srv(dev, &uv, DXGI_FORMAT_R16G16_UNORM)?];
                        (vec![y, uv], s)
                    }
                    PixelLayout::Yuv420p10 => {
                        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                        let t = vec![mk(DXGI_FORMAT_R16_UNORM, w, h)?, mk(DXGI_FORMAT_R16_UNORM, cw, ch)?, mk(DXGI_FORMAT_R16_UNORM, cw, ch)?];
                        let s = t.iter().map(|t| srv(dev, t, DXGI_FORMAT_R16_UNORM)).collect::<Result<Vec<_>>>()?;
                        (t, s)
                    }
                    PixelLayout::Nv12Cpu => {
                        let y = mk(DXGI_FORMAT_R8_UNORM, w, h)?;
                        let uv = mk(DXGI_FORMAT_R8G8_UNORM, w.div_ceil(2), h.div_ceil(2))?;
                        let s = vec![srv(dev, &y, DXGI_FORMAT_R8_UNORM)?, srv(dev, &uv, DXGI_FORMAT_R8G8_UNORM)?];
                        (vec![y, uv], s)
                    }
                    PixelLayout::Yuv420p | PixelLayout::Yuv444p => {
                        let (cw, ch) = if layout == PixelLayout::Yuv420p { (w.div_ceil(2), h.div_ceil(2)) } else { (w, h) };
                        let t = vec![mk(DXGI_FORMAT_R8_UNORM, w, h)?, mk(DXGI_FORMAT_R8_UNORM, cw, ch)?, mk(DXGI_FORMAT_R8_UNORM, cw, ch)?];
                        let s = t.iter().map(|t| srv(dev, t, DXGI_FORMAT_R8_UNORM)).collect::<Result<Vec<_>>>()?;
                        (t, s)
                    }
                    PixelLayout::Other(f) => bail!("unsupported decoded pixel format {f}"),
                };
                self.slots.push(RingSlot { textures, srvs });
            }
            self.key = Some((layout, w, h));
            self.next = 0;
        }
        let i = self.next;
        self.next = (self.next + 1) % self.slots.len();
        Ok(&self.slots[i])
    }
}

fn depth_of(layout: PixelLayout) -> Depth {
    match layout {
        PixelLayout::P010 | PixelLayout::P010Cpu => Depth::TenMsb,
        PixelLayout::Yuv420p10 => Depth::TenLsb,
        _ => Depth::Eight,
    }
}

fn kind_of(layout: PixelLayout) -> SlotKind {
    match layout {
        PixelLayout::Nv12 | PixelLayout::Nv12Cpu | PixelLayout::P010 | PixelLayout::P010Cpu => SlotKind::Nv12,
        PixelLayout::Ayuv => SlotKind::Ayuv,
        _ => SlotKind::Planar,
    }
}

fn upload(dev: &D3dDevice, ring: &mut Ring, f: &DecodedFrame) -> Result<(SlotKind, Vec<ID3D11ShaderResourceView>)> {
    let slot = ring.slot(dev, f.layout, f.width, f.height)?;
    match &f.data {
        FrameData::D3d11 { texture, index } => {
            let src = unsafe { ID3D11Texture2D::from_raw_borrowed(texture) }.ok_or_else(|| anyhow!("null frame"))?;
            let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: f.width, bottom: f.height, back: 1 };
            unsafe { dev.context.CopySubresourceRegion(&slot.textures[0], 0, 0, 0, 0, src, *index, Some(&bx)) };
        }
        FrameData::Cpu { planes, strides } => {
            for (i, tex) in slot.textures.iter().enumerate() {
                if planes[i].is_empty() {
                    continue;
                }
                unsafe {
                    dev.context.UpdateSubresource(tex, 0, None, planes[i].as_ptr() as *const _, strides[i] as u32, 0);
                }
            }
        }
    }
    Ok((kind_of(f.layout), slot.srvs.clone()))
}

fn codec_of(h: &VideoFrameHeader) -> Option<VideoCodec> {
    match h.codec() {
        pb::Codec::H264 => Some(VideoCodec::H264),
        pb::Codec::Hevc => Some(VideoCodec::Hevc),
        pb::Codec::Av1 => Some(VideoCodec::Av1),
        pb::Codec::Unspecified => None,
    }
}

pub struct VideoThread {
    /// Client window (stream slot) this decoder feeds.
    pub slot: u32,
    pub hw_allowed: bool,
    pub caps: pb::ClientCaps,
    pub store: Arc<FrameStore>,
    pub ui: Ui,
    pub net: UnboundedSender<NetCmd>,
    pub stats: Arc<Shared>,
}

impl VideoThread {
    pub fn spawn(self, dev: D3dDevice, rx: Receiver<VideoIn>) {
        std::thread::Builder::new()
            .name(format!("nya-decode-{}", self.slot))
            .spawn(move || self.run(dev, rx))
            .expect("spawn decode thread");
    }

    fn request_keyframe(&self, last: &mut Instant) {
        if last.elapsed() > Duration::from_millis(250) {
            *last = Instant::now();
            let _ = self.net.send(NetCmd::Control(pb::ControlMsg { msg: Some(Msg::RequestKeyframe(pb::RequestKeyframe { slot: self.slot })) }));
        }
    }

    /// Did capability detection find a hardware decoder for this format?
    fn hw_capable(&self, codec: VideoCodec, yuv444: bool) -> bool {
        let pc = match codec {
            VideoCodec::H264 => pb::Codec::H264,
            VideoCodec::Hevc => pb::Codec::Hevc,
            VideoCodec::Av1 => pb::Codec::Av1,
        } as i32;
        let chroma = if yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 } as i32;
        self.caps.decoders.iter().any(|d| d.codec == pc && d.chroma == chroma && d.hardware)
    }

    /// Tell the host we can't decode (codec, chroma) in hardware after all.
    fn downgrade_caps(&mut self, codec: VideoCodec, yuv444: bool) {
        let pc = match codec {
            VideoCodec::H264 => pb::Codec::H264,
            VideoCodec::Hevc => pb::Codec::Hevc,
            VideoCodec::Av1 => pb::Codec::Av1,
        } as i32;
        let chroma = if yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 } as i32;
        let mut changed = false;
        for d in &mut self.caps.decoders {
            if d.codec == pc && d.chroma == chroma && d.hardware {
                d.hardware = false;
                changed = true;
            }
        }
        if changed {
            tracing::warn!("hardware decoding of {codec:?} 444={yuv444} not available; informing host");
            let _ = self.net.send(NetCmd::Control(pb::ControlMsg { msg: Some(Msg::ClientCaps(self.caps.clone())) }));
        }
    }

    fn run(mut self, mut dev: D3dDevice, rx: Receiver<VideoIn>) {
        nya_win::com_init();
        nya_win::mmcss_boost("Playback");
        let mut dec: Option<(VideoCodec, bool, VideoDecoder)> = None;
        let mut hw_failed: HashSet<(VideoCodec, bool)> = HashSet::new();
        let mut ring = Ring::default();
        let mut expect: Option<(u64, u64)> = None; // (stream, next frame id)
        let mut need_key = true;
        let mut last_kf = Instant::now() - Duration::from_secs(1);

        for msg in rx {
            match msg {
                VideoIn::Reset => {
                    dec = None;
                    expect = None;
                    need_key = true;
                }
                VideoIn::Device(d) => {
                    dev = d;
                    ring = Ring::default();
                    dec = None;
                    need_key = true;
                    self.store.clear();
                    self.request_keyframe(&mut last_kf);
                }
                VideoIn::Frame { stream_id, buf } => {
                    let (h, payload) = match VideoFrameHeader::parse(&buf) {
                        Ok(x) => x,
                        Err(e) => {
                            tracing::warn!("bad frame header: {e}");
                            continue;
                        }
                    };
                    let Some(codec) = codec_of(&h) else { continue };
                    let yuv444 = h.chroma() == pb::Chroma::Yuv444;

                    // Gaps mean lost references: wait for a keyframe.
                    if let Some((s, next)) = expect {
                        if (s != stream_id || h.frame_id != next) && !h.is_keyframe() {
                            need_key = true;
                        }
                    }
                    expect = Some((stream_id, h.frame_id + 1));
                    if need_key && !h.is_keyframe() {
                        self.request_keyframe(&mut last_kf);
                        continue;
                    }

                    if dec.as_ref().map(|d| (d.0, d.1)) != Some((codec, yuv444)) {
                        let hw = self.hw_allowed && !hw_failed.contains(&(codec, yuv444)) && self.hw_capable(codec, yuv444);
                        // D3D11VA where the GPU has the format, else NVIDIA's NVDEC.
                        let d3d11va = crate::caps::hardware_decoders(&dev).contains(&(
                            crate::caps::pb_codec(codec),
                            if yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 },
                        ));
                        let nvdec = !d3d11va && nya_media::nvdec::supports(codec, yuv444, false);
                        let opened = if hw {
                            let first = if nvdec { VideoDecoder::new_nvdec(codec) } else { VideoDecoder::new(codec, dev.device_raw_owned()) };
                            first.or_else(|e| {
                                // Remember and fall back to software right away.
                                tracing::warn!("hardware decoder unavailable ({e:#}); using software decoding");
                                hw_failed.insert((codec, yuv444));
                                self.downgrade_caps(codec, yuv444);
                                VideoDecoder::new(codec, std::ptr::null_mut())
                            })
                        } else {
                            VideoDecoder::new(codec, std::ptr::null_mut())
                        };
                        match opened {
                            Ok(d) => {
                                self.stats.with(|s| {
                                    s.decoder = format!(
                                        "{} {}",
                                        codec.name(),
                                        match (d.is_hardware(), d.gpu_frames()) {
                                            (true, true) => "硬解",
                                            (true, false) => "硬解（NVDEC）",
                                            _ => "软解",
                                        }
                                    )
                                });
                                dec = Some((codec, yuv444, d));
                            }
                            Err(e) => {
                                tracing::error!("decoder: {e:#}");
                                continue;
                            }
                        }
                    }
                    let t = Instant::now();
                    let mut published = None;
                    let mut cpu_fallback = false;
                    let d = &mut dec.as_mut().unwrap().2;
                    let hw = d.is_hardware();
                    let gpu = d.gpu_frames();
                    let r = d.decode(payload, |f| {
                        if gpu && matches!(f.data, FrameData::Cpu { .. }) {
                            cpu_fallback = true;
                        }
                        match upload(&dev, &mut ring, f) {
                            Ok((kind, srvs)) => {
                                published = Some(Slot {
                                    kind,
                                    width: f.width,
                                    height: f.height,
                                    full_range: f.full_range,
                                    matrix: f.matrix,
                                    depth: depth_of(f.layout),
                                    pq: f.pq,
                                    srvs,
                                    capture_ts: h.capture_ts_us,
                                })
                            }
                            Err(e) => tracing::warn!("upload: {e:#}"),
                        }
                    });
                    if cpu_fallback && !hw_failed.contains(&(codec, yuv444)) {
                        hw_failed.insert((codec, yuv444));
                        self.stats.with(|s| s.decoder = format!("{} 软解（硬件不支持此格式）", codec.name()));
                        self.downgrade_caps(codec, yuv444);
                    }
                    match r {
                        Ok(()) => {
                            if h.is_keyframe() {
                                need_key = false;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("decode error: {e:#}");
                            if hw && hw_failed.insert((codec, yuv444)) {
                                self.downgrade_caps(codec, yuv444);
                            }
                            dec = None;
                            need_key = true;
                            self.request_keyframe(&mut last_kf);
                            continue;
                        }
                    }
                    let decode_ms = t.elapsed().as_secs_f32() * 1000.0;
                    if let Some(slot) = published {
                        let dropped = self.store.put(Arc::new(slot));
                        let first = self.stats.with(|s| {
                            s.total_decoded += 1;
                            s.total_decoded == 1
                        });
                        if first {
                            tracing::info!("first frame decoded ({:?}, {:.1} ms)", self.store.take_kind(), decode_ms);
                        }
                        self.stats.with(|s| {
                            s.decode_ms.push(decode_ms);
                            s.frames_decoded += 1;
                            if dropped {
                                s.frames_dropped += 1;
                            }
                        });
                        self.ui.send(UiEvent::Frame(self.slot));
                    }
                }
            }
        }
    }
}
