//! Presentation: flip-model swap chain, YUV→RGB shaders and letterboxing.
//! The UI is painted between [`Renderer::draw_video`] and [`Renderer::present`].
//!
//! When the window's monitor is in HDR mode the swap chain is FP16 scRGB:
//! HDR10 video keeps its highlights, SDR video and the UI sit at the
//! monitor's SDR white. The UI is then painted into an 8-bit layer that
//! [`Renderer::present`] blends on top. Elsewhere HDR10 video is tone-mapped.

use anyhow::{anyhow, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};

use crate::video::{Depth, Slot, SlotKind};
use nya_media::decoder::Matrix;
use nya_win::d3d::{compile_shader, tex_desc, D3dDevice};

const HLSL: &str = include_str!("shaders/render.hlsl");

/// Brightness HDR10 video's SDR white is shown at on an SDR screen (BT.2408).
const PQ_REFERENCE_WHITE_NITS: f32 = 203.0;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Cb {
    dst: [f32; 4],
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
    off: [f32; 4],
    scale: [f32; 4],
    xfer: [f32; 4],
}

/// Rectangle in window pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Fit `vw`×`vh` into the window keeping the aspect ratio.
pub fn fit(win_w: u32, win_h: u32, vw: u32, vh: u32) -> Rect {
    if vw == 0 || vh == 0 || win_w == 0 || win_h == 0 {
        return Rect { x: 0.0, y: 0.0, w: win_w as f64, h: win_h as f64 };
    }
    let s = (win_w as f64 / vw as f64).min(win_h as f64 / vh as f64);
    let (w, h) = ((vw as f64 * s).round(), (vh as f64 * s).round());
    Rect { x: ((win_w as f64 - w) / 2.0).floor(), y: ((win_h as f64 - h) / 2.0).floor(), w, h }
}

type Params = ([f32; 4], [f32; 4], [f32; 4], [f32; 4], [f32; 4]);

fn yuv_params(matrix: Matrix, full_range: bool, depth: Depth) -> Params {
    let (m0, m1, m2) = match matrix {
        Matrix::Bt709 => ([1.0, 0.0, 1.5748, 0.0], [1.0, -0.187324, -0.468124, 0.0], [1.0, 1.8556, 0.0, 0.0]),
        Matrix::Bt601 => ([1.0, 0.0, 1.402, 0.0], [1.0, -0.344136, -0.714136, 0.0], [1.0, 1.772, 0.0, 0.0]),
        Matrix::Bt2020 => ([1.0, 0.0, 1.4746, 0.0], [1.0, -0.164553, -0.571353, 0.0], [1.0, 1.8814, 0.0, 0.0]),
    };
    // A sample of 1.0 in the shader is this code value.
    let (bits, k): (u32, f32) = match depth {
        Depth::Eight => (8, 255.0),
        Depth::TenMsb => (10, 65535.0 / 64.0),
        Depth::TenLsb => (10, 65535.0),
    };
    let sh = (1u32 << (bits - 8)) as f32;
    let (off, scale) = if full_range {
        let max = ((1u32 << bits) - 1) as f32;
        let half = (1u32 << (bits - 1)) as f32;
        ([0.0, half / k, half / k, 0.0], [k / max, k / max, k / max, 0.0])
    } else {
        ([16.0 * sh / k, 128.0 * sh / k, 128.0 * sh / k, 0.0], [k / (219.0 * sh), k / (224.0 * sh), k / (224.0 * sh), 0.0])
    };
    (m0, m1, m2, off, scale)
}

/// The monitor `hwnd` is on, if it is in HDR mode: its SDR white (nits).
fn monitor_hdr(dev: &D3dDevice, hwnd: HWND) -> Option<f32> {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let dxgi: IDXGIDevice = dev.device.cast().ok()?;
        let factory: IDXGIFactory1 = dxgi.GetAdapter().ok()?.GetParent().ok()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.Monitor == mon {
                        if !nya_win::topology::is_hdr(&output) {
                            return None;
                        }
                        let n = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
                        let name = String::from_utf16_lossy(&desc.DeviceName[..n]);
                        return Some(nya_win::display_config::sdr_white_nits(&name).unwrap_or(80.0));
                    }
                }
                o += 1;
            }
            a += 1;
        }
        None
    }
}

/// The 8-bit UI layer of an HDR swap chain.
struct Overlay {
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
}

pub struct Renderer {
    pub dev: D3dDevice,
    hwnd: HWND,
    swap: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    pub width: u32,
    pub height: u32,
    vs: ID3D11VertexShader,
    ps_nv12: ID3D11PixelShader,
    ps_ayuv: ID3D11PixelShader,
    ps_planar: ID3D11PixelShader,
    ps_overlay: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cbuf: ID3D11Buffer,
    premultiplied: ID3D11BlendState,
    tearing_supported: bool,
    /// HDR swap chain (FP16 scRGB): the monitor's SDR white in nits.
    hdr_out: Option<f32>,
    overlay: Option<Overlay>,
}

impl Renderer {
    pub fn new(dev: D3dDevice, hwnd: HWND, width: u32, height: u32) -> Result<Self> {
        let d = &dev.device;
        unsafe {
            let dxgi_dev: IDXGIDevice1 = d.cast()?;
            let _ = dxgi_dev.SetMaximumFrameLatency(1);
            let adapter = dxgi_dev.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            let tearing_supported = factory
                .cast::<IDXGIFactory5>()
                .map(|f5| {
                    let mut allow = BOOL(0);
                    f5.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        &mut allow as *mut _ as *mut _,
                        std::mem::size_of::<BOOL>() as u32,
                    )
                    .is_ok()
                        && allow.as_bool()
                })
                .unwrap_or(false);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width.max(1),
                Height: height.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
                Flags: if tearing_supported { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32 } else { 0 },
                ..Default::default()
            };
            let swap = factory.CreateSwapChainForHwnd(d, hwnd, &desc, None, None)?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

            let vs_code = compile_shader(HLSL, "vs_quad", "vs_4_0")?;
            let mut vs = None;
            d.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            let ps = |e: &str| -> Result<ID3D11PixelShader> {
                let code = compile_shader(HLSL, e, "ps_4_0")?;
                let mut p = None;
                d.CreatePixelShader(&code, None, Some(&mut p))?;
                p.ok_or_else(|| anyhow!("pixel shader {e}"))
            };
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            d.CreateSamplerState(&sd, Some(&mut sampler))?;
            let bd = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<Cb>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut cbuf = None;
            d.CreateBuffer(&bd, None, Some(&mut cbuf))?;
            let mut blend = D3D11_BLEND_DESC::default();
            blend.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut premultiplied = None;
            d.CreateBlendState(&blend, Some(&mut premultiplied))?;

            let mut r = Self {
                swap,
                hwnd,
                rtv: None,
                width: width.max(1),
                height: height.max(1),
                vs: vs.unwrap(),
                ps_nv12: ps("ps_nv12")?,
                ps_ayuv: ps("ps_ayuv")?,
                ps_planar: ps("ps_planar")?,
                ps_overlay: ps("ps_overlay")?,
                sampler: sampler.unwrap(),
                cbuf: cbuf.unwrap(),
                premultiplied: premultiplied.unwrap(),
                tearing_supported,
                hdr_out: None,
                overlay: None,
                dev,
            };
            r.refresh_display();
            Ok(r)
        }
    }

    /// The window's monitor is in HDR mode (the swap chain is HDR).
    pub fn display_hdr(&self) -> bool {
        self.hdr_out.is_some()
    }

    /// Follow the window's monitor into or out of HDR mode (it moved, or HDR
    /// was switched in Windows). Returns true when it changed.
    pub fn refresh_display(&mut self) -> bool {
        let want = monitor_hdr(&self.dev, self.hwnd);
        if want == self.hdr_out {
            return false;
        }
        let (fmt, space) = match want {
            Some(_) => (DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709),
            None => (DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709),
        };
        self.rtv = None;
        self.overlay = None;
        let r = unsafe {
            self.dev.context.OMSetRenderTargets(None, None);
            self.dev.context.Flush();
            let flags = if self.tearing_supported { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { DXGI_SWAP_CHAIN_FLAG(0) };
            self.swap
                .ResizeBuffers(0, self.width, self.height, fmt, flags)
                .and_then(|()| self.swap.cast::<IDXGISwapChain3>()?.SetColorSpace1(space))
        };
        match r {
            Ok(()) => {
                tracing::info!(
                    "window display: {}",
                    match want {
                        Some(w) => format!("HDR (SDR white {w:.0} nits), FP16 scRGB output"),
                        None => "SDR".into(),
                    }
                );
                self.hdr_out = want;
                true
            }
            Err(e) => {
                tracing::warn!("switching the swap chain for HDR: {e}");
                false
            }
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        self.rtv = None;
        self.overlay = None;
        unsafe {
            self.dev.context.OMSetRenderTargets(None, None);
            self.dev.context.Flush();
            let flags = if self.tearing_supported { DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING } else { DXGI_SWAP_CHAIN_FLAG(0) };
            self.swap.ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, flags)?;
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn ensure_rtv(&mut self) -> Result<ID3D11RenderTargetView> {
        if let Some(r) = &self.rtv {
            return Ok(r.clone());
        }
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            let mut rtv = None;
            self.dev.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
            let rtv = rtv.ok_or_else(|| anyhow!("CreateRenderTargetView"))?;
            self.rtv = Some(rtv.clone());
            Ok(rtv)
        }
    }

    fn ensure_overlay(&mut self) -> Result<ID3D11RenderTargetView> {
        if let Some(o) = &self.overlay {
            return Ok(o.rtv.clone());
        }
        let bind = D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0);
        let tex = self.dev.texture(&tex_desc(self.width, self.height, DXGI_FORMAT_B8G8R8A8_UNORM, bind))?;
        let mut rtv = None;
        unsafe { self.dev.device.CreateRenderTargetView(&tex, None, Some(&mut rtv))? };
        let rtv = rtv.ok_or_else(|| anyhow!("overlay RTV"))?;
        let srv = self.dev.srv(&tex)?;
        self.overlay = Some(Overlay { rtv: rtv.clone(), srv });
        Ok(rtv)
    }

    fn ndc(&self, r: Rect) -> [f32; 4] {
        let (w, h) = (self.width as f64, self.height as f64);
        [
            (r.x / w * 2.0 - 1.0) as f32,
            (1.0 - r.y / h * 2.0) as f32,
            ((r.x + r.w) / w * 2.0 - 1.0) as f32,
            (1.0 - (r.y + r.h) / h * 2.0) as f32,
        ]
    }

    /// Clear the back buffer; returns the view the UI paints into (the back
    /// buffer, or on an HDR swap chain the 8-bit UI layer).
    pub fn begin(&mut self) -> Result<ID3D11RenderTargetView> {
        let rtv = self.ensure_rtv()?;
        unsafe {
            self.dev.context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.dev.context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
        }
        if self.hdr_out.is_none() {
            return Ok(rtv);
        }
        let ui = self.ensure_overlay()?;
        unsafe { self.dev.context.ClearRenderTargetView(&ui, &[0.0, 0.0, 0.0, 0.0]) };
        Ok(ui)
    }

    fn xfer(&self, pq: bool) -> [f32; 4] {
        let white = self.hdr_out.unwrap_or(80.0);
        [pq as u8 as f32, self.hdr_out.is_some() as u8 as f32, white / 80.0, PQ_REFERENCE_WHITE_NITS]
    }

    fn setup_pass(&self, ctx: &ID3D11DeviceContext, rtv: &ID3D11RenderTargetView) {
        unsafe {
            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.width as f32,
                Height: self.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[vp]));
            ctx.RSSetState(None);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        }
    }

    /// Draw a decoded frame letterboxed into the window.
    pub fn draw_video(&mut self, s: &Slot) {
        let Some(back) = self.rtv.clone() else { return };
        let ctx = self.dev.context.clone();
        let r = fit(self.width, self.height, s.width, s.height);
        let (m0, m1, m2, off, scale) = yuv_params(s.matrix, s.full_range, s.depth);
        let cb = Cb { dst: self.ndc(r), m0, m1, m2, off, scale, xfer: self.xfer(s.pq) };
        let ps = match s.kind {
            SlotKind::Nv12 => &self.ps_nv12,
            SlotKind::Ayuv => &self.ps_ayuv,
            SlotKind::Planar => &self.ps_planar,
        };
        let views: Vec<Option<ID3D11ShaderResourceView>> = s.srvs.iter().cloned().map(Some).collect();
        self.setup_pass(&ctx, &back);
        unsafe {
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
            ctx.UpdateSubresource(&self.cbuf, 0, None, &cb as *const _ as *const _, 0, 0);
            ctx.PSSetShader(ps, None);
            ctx.PSSetShaderResources(0, Some(&views));
            ctx.Draw(4, 0);
            ctx.PSSetShaderResources(0, Some(&[None, None, None]));
        }
    }

    /// HDR swap chain: blend the UI layer onto the back buffer.
    fn composite_overlay(&mut self) {
        let (Some(back), Some(ov)) = (self.rtv.clone(), self.overlay.as_ref().map(|o| o.srv.clone())) else { return };
        let ctx = self.dev.context.clone();
        let cb = Cb { dst: [-1.0, 1.0, 1.0, -1.0], xfer: self.xfer(false), ..Default::default() };
        self.setup_pass(&ctx, &back);
        unsafe {
            ctx.OMSetBlendState(&self.premultiplied, None, 0xffff_ffff);
            ctx.UpdateSubresource(&self.cbuf, 0, None, &cb as *const _ as *const _, 0, 0);
            ctx.PSSetShader(&self.ps_overlay, None);
            ctx.PSSetShaderResources(0, Some(&[Some(ov)]));
            ctx.Draw(4, 0);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
        }
    }

    pub fn present(&mut self, allow_tearing: bool) -> Result<()> {
        if self.hdr_out.is_some() {
            self.composite_overlay();
        }
        let tear = allow_tearing && self.tearing_supported;
        let hr = unsafe { self.swap.Present(0, if tear { DXGI_PRESENT_ALLOW_TEARING } else { DXGI_PRESENT(0) }) };
        if hr.is_err() {
            return Err(anyhow!("Present: {hr:?}"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes() {
        let r = fit(1920, 1200, 1920, 1080);
        assert_eq!((r.x, r.y, r.w, r.h), (0.0, 60.0, 1920.0, 1080.0));
        let r = fit(1000, 1000, 2000, 1000);
        assert_eq!((r.w, r.h), (1000.0, 500.0));
    }

    #[test]
    fn shaders_compile() {
        for (e, t) in [
            ("vs_quad", "vs_4_0"),
            ("ps_nv12", "ps_4_0"),
            ("ps_ayuv", "ps_4_0"),
            ("ps_planar", "ps_4_0"),
            ("ps_overlay", "ps_4_0"),
        ] {
            compile_shader(HLSL, e, t).unwrap();
        }
    }

    fn convert(p: Params, y: f32, u: f32, v: f32) -> [f32; 3] {
        let (m0, m1, m2, off, scale) = p;
        let yuv = [(y - off[0]) * scale[0], (u - off[1]) * scale[1], (v - off[2]) * scale[2]];
        let d = |m: [f32; 4]| m[0] * yuv[0] + m[1] * yuv[1] + m[2] * yuv[2];
        [d(m0), d(m1), d(m2)]
    }

    /// Limited-range white and black map to 1 and 0 at every depth.
    #[test]
    fn yuv_matrix() {
        let close = |c: [f32; 3], want: f32| c.iter().all(|x| (x - want).abs() < 2e-3);
        let p = yuv_params(Matrix::Bt709, false, Depth::Eight);
        assert!(close(convert(p, 235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0), 1.0));
        assert!(close(convert(p, 16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0), 0.0));
        // P010: 10-bit codes in the top bits of a 16-bit unorm.
        let msb = |code: f32| code * 64.0 / 65535.0;
        let p = yuv_params(Matrix::Bt2020, false, Depth::TenMsb);
        assert!(close(convert(p, msb(940.0), msb(512.0), msb(512.0)), 1.0));
        assert!(close(convert(p, msb(64.0), msb(512.0), msb(512.0)), 0.0));
        // yuv420p10le: the same codes in the low bits.
        let lsb = |code: f32| code / 65535.0;
        let p = yuv_params(Matrix::Bt2020, false, Depth::TenLsb);
        assert!(close(convert(p, lsb(940.0), lsb(512.0), lsb(512.0)), 1.0));
        // BT.2020 pure red: Cr at its maximum gives R' = 1.
        let p = yuv_params(Matrix::Bt2020, false, Depth::TenMsb);
        let y = 64.0 + 876.0 * 0.2627;
        let cb = 512.0 + 896.0 * (-0.2627 / 1.8814);
        let cr = 512.0 + 896.0 * ((1.0 - 0.2627) / 1.4746);
        let rgb = convert(p, msb(y), msb(cb), msb(cr));
        assert!((rgb[0] - 1.0).abs() < 2e-3 && rgb[1].abs() < 2e-3 && rgb[2].abs() < 2e-3, "{rgb:?}");
    }
}
