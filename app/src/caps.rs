//! What this client can decode, reported to the host in `ClientCaps`.

use nya_proto::pb;
use nya_win::d3d::D3dDevice;
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11VideoDevice, D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, D3D11_DECODER_PROFILE_H264_VLD_NOFGT,
    D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010};

/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN_444` (d3d11.h, Windows SDK 10.0.26100).
const HEVC_MAIN_444: GUID = GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33);

pub fn pb_codec(c: nya_media::VideoCodec) -> pb::Codec {
    match c {
        nya_media::VideoCodec::H264 => pb::Codec::H264,
        nya_media::VideoCodec::Hevc => pb::Codec::Hevc,
        nya_media::VideoCodec::Av1 => pb::Codec::Av1,
    }
}

fn cap(codec: pb::Codec, chroma: pb::Chroma, hardware: bool) -> pb::CodecCap {
    pb::CodecCap { codec: codec as i32, chroma: chroma as i32, hardware, ..Default::default() }
}

/// HEVC Main10 (HDR10) decodes in hardware on `dev`.
pub fn hardware_hevc_main10(dev: &D3dDevice) -> bool {
    let Ok(vd) = dev.device.cast::<ID3D11VideoDevice>() else { return false };
    let g = D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10;
    let n = unsafe { vd.GetVideoDecoderProfileCount() };
    (0..n).any(|i| unsafe { vd.GetVideoDecoderProfile(i) }.is_ok_and(|p| p == g))
        && unsafe { vd.CheckVideoDecoderFormat(&g, DXGI_FORMAT_P010) }.is_ok_and(|b| b.as_bool())
}

/// Hardware decoder profiles supported by `dev`, as (codec, chroma).
pub fn hardware_decoders(dev: &D3dDevice) -> Vec<(pb::Codec, pb::Chroma)> {
    let mut out = Vec::new();
    let Ok(vd) = dev.device.cast::<ID3D11VideoDevice>() else { return out };
    let check = |g: &GUID, f: DXGI_FORMAT| unsafe { vd.CheckVideoDecoderFormat(g, f).map(|b| b.as_bool()).unwrap_or(false) };
    let n = unsafe { vd.GetVideoDecoderProfileCount() };
    for i in 0..n {
        let Ok(g) = (unsafe { vd.GetVideoDecoderProfile(i) }) else { continue };
        let entry = if g == D3D11_DECODER_PROFILE_H264_VLD_NOFGT && check(&g, DXGI_FORMAT_NV12) {
            (pb::Codec::H264, pb::Chroma::Yuv420)
        } else if g == D3D11_DECODER_PROFILE_HEVC_VLD_MAIN && check(&g, DXGI_FORMAT_NV12) {
            (pb::Codec::Hevc, pb::Chroma::Yuv420)
        } else if g == HEVC_MAIN_444 && check(&g, DXGI_FORMAT_AYUV) {
            (pb::Codec::Hevc, pb::Chroma::Yuv444)
        } else if g == D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0 && check(&g, DXGI_FORMAT_NV12) {
            (pb::Codec::Av1, pb::Chroma::Yuv420)
        } else {
            continue;
        };
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

pub fn detect(dev: &D3dDevice, hw_allowed: bool, max_fps: u32) -> pb::ClientCaps {
    let hw = if hw_allowed { hardware_decoders(dev) } else { Vec::new() };
    let mut decoders: Vec<pb::CodecCap> = hw.iter().map(|&(c, ch)| cap(c, ch, true)).collect();
    // FFmpeg can decode all of these in software.
    for (c, ch) in [
        (pb::Codec::H264, pb::Chroma::Yuv420),
        (pb::Codec::Hevc, pb::Chroma::Yuv420),
        (pb::Codec::H264, pb::Chroma::Yuv444),
        (pb::Codec::Hevc, pb::Chroma::Yuv444),
        (pb::Codec::Av1, pb::Chroma::Yuv420),
    ] {
        if !hw.contains(&(c, ch)) {
            decoders.push(cap(c, ch, false));
        }
    }
    // Formats D3D11VA lacks but NVIDIA's NVDEC has (e.g. HEVC 4:4:4) are hardware too.
    if hw_allowed {
        for f in nya_media::nvdec::formats().iter().filter(|f| !f.ten_bit) {
            let chroma = if f.yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 };
            let codec = pb_codec(f.codec);
            if let Some(d) = decoders.iter_mut().find(|d| d.codec == codec as i32 && d.chroma == chroma as i32 && !d.ten_bit) {
                d.hardware = true;
            }
        }
    }
    // HDR10: HEVC Main10, in hardware when the GPU can, else in software.
    let main10_hw = hw_allowed && (hardware_hevc_main10(dev) || nya_media::nvdec::supports(nya_media::VideoCodec::Hevc, false, true));
    decoders.push(pb::CodecCap { ten_bit: true, ..cap(pb::Codec::Hevc, pb::Chroma::Yuv420, main10_hw) });
    pb::ClientCaps { decoders, max_width: 0, max_height: 0, max_fps }
}
