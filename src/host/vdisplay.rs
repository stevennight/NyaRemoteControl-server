//! Virtual display for a session (Virtual Display Driver, optional component).
//!
//! The driver's device stays disabled while nobody uses it. A session that
//! asks for a virtual display gets it enabled at the requested resolution and
//! made the primary display; in privacy mode it becomes the only display (the
//! physical ones go dark) and the host's own keyboard and mouse are blocked.
//! Dropping [`VirtualDisplay`] disables the device again and lets Windows
//! re-apply the display layout it had stored for the physical monitors.
//!
//! The driver only offers the resolutions listed in its settings file, which
//! it reads when it starts: a size that is not listed yet means rewriting the
//! file and restarting the device (the virtual screen blinks once).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use nya_proto::pb;
use nya_win::devnode;
use nya_win::display_config::{self as dc, Config, MODE_IDX_INVALID, PATH_ACTIVE};
use nya_win::input_block::InputBlocker;
use windows::Win32::Devices::Display::{DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO};
use windows::Win32::Foundation::LUID;

use crate::components::{VDD_HWID, VDD_SETTINGS_DIR};

/// Resolutions always offered, so switching between common sizes needs no
/// driver restart.
const COMMON: [(u32, u32); 16] = [
    (1280, 720),
    (1280, 800),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2560, 1080),
    (2560, 1440),
    (2560, 1600),
    (2880, 1800),
    (3440, 1440),
    (3840, 1600),
    (3840, 2160),
    (3840, 2400),
];
const RATES: [u32; 8] = [60, 75, 90, 100, 120, 144, 165, 240];

/// What the client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want {
    pub private: bool,
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    /// Windows scaling in percent; 0 = leave as is.
    pub scale: u32,
}

impl Want {
    /// `None` = physical displays (no virtual display).
    pub fn from_pb(v: Option<&pb::VirtualDisplay>) -> Option<Self> {
        let v = v?;
        let mode = pb::DisplayMode::try_from(v.mode).unwrap_or(pb::DisplayMode::Physical);
        if mode == pb::DisplayMode::Physical {
            return None;
        }
        let (width, height) = if v.width >= 640 && v.height >= 480 {
            (v.width.min(7680) & !1, v.height.min(4320) & !1)
        } else {
            (1920, 1080)
        };
        Some(Self {
            private: mode == pb::DisplayMode::Private,
            width,
            height,
            hz: if v.refresh_hz == 0 { 60 } else { v.refresh_hz.clamp(24, 240) },
            scale: v.scale_percent.min(500),
        })
    }
}

/// Is the driver installed?
pub fn available() -> bool {
    devnode::exists(VDD_HWID)
}

pub struct VirtualDisplay {
    want: Want,
    /// `\\.\DISPLAYn` of the virtual display.
    pub gdi_name: String,
    instance: String,
    /// Sizes requested during this session (kept in the driver's list).
    extra: Vec<(u32, u32)>,
    scale_set: u32,
    blocker: Option<InputBlocker>,
}

impl VirtualDisplay {
    pub fn open(want: Want) -> Result<Self> {
        let instance = devnode::instance_ids(VDD_HWID)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("被控端没有安装虚拟显示器驱动（在被控端管理界面的“可选组件”中安装）"))?;
        let mut vd = Self { want, gdi_name: String::new(), instance, extra: Vec::new(), scale_set: 0, blocker: None };
        // On error, dropping `vd` restores the physical displays.
        vd.setup()?;
        Ok(vd)
    }

    pub fn want(&self) -> Want {
        self.want
    }

    /// Apply a changed request (resolution, privacy, scaling).
    pub fn update(&mut self, want: Want) -> Result<()> {
        if want == self.want {
            return Ok(());
        }
        if self.want.private && !want.private {
            // The physical displays were switched off: bring back their
            // stored layout first, then add the virtual display to it.
            self.blocker = None;
            release();
        }
        self.want = want;
        self.setup()
    }

    /// Bring the device and the display layout to what `self.want` says.
    /// Idempotent.
    fn setup(&mut self) -> Result<()> {
        let t0 = Instant::now();
        let w = self.want;
        let size = (w.width, w.height);
        if !COMMON.contains(&size) && !self.extra.contains(&size) {
            self.extra.push(size);
        }
        let changed = write_settings(&self.extra, w.hz).context("写入虚拟显示器设置")?;
        if changed && devnode::is_started(VDD_HWID) {
            tracing::info!("virtual display: new resolution list; restarting the driver");
            devnode::set_enabled(VDD_HWID, false).context("停用虚拟显示器")?;
            std::thread::sleep(Duration::from_millis(500));
            self.scale_set = 0; // a new monitor comes up
        }
        devnode::set_enabled(VDD_HWID, true).context("启用虚拟显示器（需要管理员权限，开发模式下不可用）")?;
        let luid = wait_for(Duration::from_secs(10), || vdd_adapter(&self.instance).filter(|&l| has_target(l)))
            .ok_or_else(|| anyhow!("虚拟显示器驱动已启用，但没有出现显示器"))?;

        activate(luid, w.private).context("切换显示器布局")?;
        self.gdi_name = wait_for(Duration::from_secs(5), || vdd_gdi_name(luid))
            .ok_or_else(|| anyhow!("虚拟显示器没有进入桌面"))?;
        if let Err(e) = dc::set_mode(&self.gdi_name, w.width, w.height, w.hz) {
            tracing::warn!("virtual display mode {}x{}@{}: {e:#}", w.width, w.height, w.hz);
        }
        if !w.private {
            if let Err(e) = arrange(luid) {
                tracing::warn!("virtual display: arrange displays: {e:#}");
            }
        }
        if w.scale > 0 && w.scale != self.scale_set {
            match dc::set_scale(&self.gdi_name, w.scale) {
                Ok(s) => {
                    self.scale_set = w.scale;
                    tracing::info!("virtual display scaling {s}%");
                }
                Err(e) => tracing::warn!("virtual display scaling {}%: {e:#}", w.scale),
            }
        }
        // The capture side needs DXGI to see the new output.
        let _ = wait_for(Duration::from_secs(3), || {
            nya_win::topology::Topology::enumerate()
                .ok()
                .and_then(|t| t.outputs.iter().any(|o| o.device_name.eq_ignore_ascii_case(&self.gdi_name)).then_some(()))
        });
        match (w.private, self.blocker.is_some()) {
            (true, false) => self.blocker = Some(InputBlocker::start()),
            (false, true) => self.blocker = None,
            _ => {}
        }
        tracing::info!(
            "virtual display {} {}x{}@{}{} ready in {} ms",
            self.gdi_name,
            w.width,
            w.height,
            w.hz,
            if w.private { " (privacy: physical displays off, local input blocked)" } else { "" },
            t0.elapsed().as_millis()
        );
        Ok(())
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        self.blocker = None;
        release();
    }
}

/// Disable the driver's device and restore the physical layout.
fn release() {
    match devnode::set_enabled(VDD_HWID, false) {
        Ok(_) => tracing::info!("virtual display removed"),
        Err(e) => tracing::warn!("disable virtual display: {e:#}"),
    }
    // Windows usually re-applies the stored layout by itself; make sure, also
    // when the virtual display was the only active one.
    std::thread::sleep(Duration::from_millis(300));
    if let Err(e) = dc::restore_database() {
        tracing::warn!("restore display layout: {e:#}");
    }
}

/// A previous helper died while a virtual display was up (physical displays
/// may still be off): undo it. Called when the host starts.
pub fn cleanup_stale() {
    if devnode::is_started(VDD_HWID) {
        tracing::warn!("virtual display left enabled by a previous run; restoring the physical displays");
        release();
    }
}

fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let end = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn luid_eq(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}

/// Adapter LUID of the driver's device (the indirect display adapter).
fn vdd_adapter(instance: &str) -> Option<LUID> {
    let cfg = Config::query(true).ok()?;
    let mut seen: Vec<LUID> = Vec::new();
    for p in &cfg.paths {
        let l = p.targetInfo.adapterId;
        if seen.iter().any(|s| luid_eq(*s, l)) {
            continue;
        }
        seen.push(l);
        if dc::adapter_path(l).is_some_and(|path| dc::adapter_path_matches(&path, instance)) {
            return Some(l);
        }
    }
    None
}

fn has_target(luid: LUID) -> bool {
    Config::query(true)
        .map(|c| c.paths.iter().any(|p| luid_eq(p.targetInfo.adapterId, luid) && p.targetInfo.targetAvailable.as_bool()))
        .unwrap_or(false)
}

fn vdd_gdi_name(luid: LUID) -> Option<String> {
    let cfg = Config::query(false).ok()?;
    let found = cfg.active().find(|p| luid_eq(p.targetInfo.adapterId, luid)).and_then(dc::source_gdi_name);
    found
}

fn same_path(a: &DISPLAYCONFIG_PATH_INFO, b: &DISPLAYCONFIG_PATH_INFO) -> bool {
    luid_eq(a.targetInfo.adapterId, b.targetInfo.adapterId)
        && a.targetInfo.id == b.targetInfo.id
        && luid_eq(a.sourceInfo.adapterId, b.sourceInfo.adapterId)
        && a.sourceInfo.id == b.sourceInfo.id
}

/// Make the virtual display active; in privacy mode, the only active display.
fn activate(luid: LUID, private: bool) -> Result<()> {
    let all = Config::query(true)?;
    let on_vdd = |p: &DISPLAYCONFIG_PATH_INFO| luid_eq(p.targetInfo.adapterId, luid);
    let mut paths: Vec<_> = all.active().copied().collect();
    if !paths.iter().any(on_vdd) {
        let used: Vec<u32> = paths.iter().filter(|p| luid_eq(p.sourceInfo.adapterId, luid)).map(|p| p.sourceInfo.id).collect();
        let mut p = *all
            .paths
            .iter()
            .find(|p| on_vdd(p) && p.targetInfo.targetAvailable.as_bool() && !used.contains(&p.sourceInfo.id))
            .ok_or_else(|| anyhow!("no usable path to the virtual display"))?;
        p.flags |= PATH_ACTIVE;
        p.sourceInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
        p.targetInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
        paths.push(p);
    }
    if private {
        paths.retain(|p| on_vdd(p));
    }
    let unchanged = paths.len() == all.active().count() && all.active().all(|a| paths.iter().any(|p| same_path(p, a)));
    if unchanged {
        return Ok(());
    }
    tracing::info!("display layout: {} active path(s) -> {}", all.active().count(), paths.len());
    dc::apply(&paths, &all.modes)
}

/// Extended mode: the virtual display at (0, 0), which makes it primary, and
/// the physical displays to its right in their existing arrangement.
fn arrange(luid: LUID) -> Result<()> {
    let cfg = Config::query(false)?;
    let mut modes = cfg.modes.clone();
    let src_idx = |p: &DISPLAYCONFIG_PATH_INFO| unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
    let vdd = cfg.active().find(|p| luid_eq(p.targetInfo.adapterId, luid)).ok_or_else(|| anyhow!("virtual display not active"))?;
    let (vw, _, vx, vy) = cfg.source_mode(vdd).ok_or_else(|| anyhow!("no source mode"))?;
    let mut others: Vec<usize> = cfg.active().filter(|p| !luid_eq(p.targetInfo.adapterId, luid)).map(src_idx).collect();
    others.sort();
    others.dedup();
    let others: Vec<usize> = others.into_iter().filter(|&i| i != src_idx(vdd) && i < modes.len()).collect();
    let pos = |m: &DISPLAYCONFIG_MODE_INFO| unsafe { m.Anonymous.sourceMode.position };
    let min_x = others.iter().map(|&i| pos(&modes[i]).x).min().unwrap_or(0);
    let min_y = others.iter().map(|&i| pos(&modes[i]).y).min().unwrap_or(0);
    let done = vx == 0 && vy == 0 && (others.is_empty() || (min_x == vw as i32 && min_y == 0));
    if done {
        return Ok(());
    }
    unsafe {
        let v = &mut modes[src_idx(vdd)].Anonymous.sourceMode.position;
        v.x = 0;
        v.y = 0;
        for &i in &others {
            let p = &mut modes[i].Anonymous.sourceMode.position;
            p.x = p.x - min_x + vw as i32;
            p.y -= min_y;
        }
    }
    dc::apply(&cfg.paths, &modes)
}

fn settings_xml(sizes: &[(u32, u32)], hz: u32) -> String {
    let mut rates: Vec<u32> = RATES.to_vec();
    if !rates.contains(&hz) {
        rates.push(hz);
        rates.sort();
    }
    let mut x = String::from(
        "<?xml version='1.0' encoding='utf-8'?>\n<!-- Written by NyaRemoteControl for each session; edits are overwritten. -->\n<vdd_settings>\n    <monitors>\n        <count>1</count>\n    </monitors>\n    <gpu>\n        <friendlyname>default</friendlyname>\n    </gpu>\n    <global>\n",
    );
    for r in &rates {
        x += &format!("        <g_refresh_rate>{r}</g_refresh_rate>\n");
    }
    x += "    </global>\n    <resolutions>\n";
    for (w, h) in COMMON.iter().chain(sizes.iter()) {
        x += &format!(
            "        <resolution>\n            <width>{w}</width>\n            <height>{h}</height>\n            <refresh_rate>60</refresh_rate>\n        </resolution>\n"
        );
    }
    x += "    </resolutions>\n    <options>\n        <CustomEdid>false</CustomEdid>\n        <PreventSpoof>false</PreventSpoof>\n        <EdidCeaOverride>false</EdidCeaOverride>\n        <HardwareCursor>true</HardwareCursor>\n        <SDR10bit>false</SDR10bit>\n        <HDRPlus>false</HDRPlus>\n        <logging>false</logging>\n        <debuglogging>false</debuglogging>\n    </options>\n</vdd_settings>\n";
    x
}

/// Write the driver's settings; true if the file changed.
fn write_settings(sizes: &[(u32, u32)], hz: u32) -> Result<bool> {
    let path = Path::new(VDD_SETTINGS_DIR).join("vdd_settings.xml");
    let xml = settings_xml(sizes, hz);
    if std::fs::read_to_string(&path).is_ok_and(|old| old == xml) {
        return Ok(false);
    }
    std::fs::create_dir_all(VDD_SETTINGS_DIR)?;
    std::fs::write(&path, xml).with_context(|| format!("{}", path.display()))?;
    if !devnode::is_started(VDD_HWID) {
        return Ok(false); // read when the device starts
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn want_from_request() {
        assert_eq!(Want::from_pb(None), None);
        assert_eq!(Want::from_pb(Some(&pb::VirtualDisplay::default())), None);
        let w = Want::from_pb(Some(&pb::VirtualDisplay {
            mode: pb::DisplayMode::Private as i32,
            width: 1917,
            height: 1043,
            refresh_hz: 0,
            scale_percent: 150,
        }))
        .unwrap();
        assert_eq!((w.private, w.width, w.height, w.hz, w.scale), (true, 1916, 1042, 60, 150));
        let w = Want::from_pb(Some(&pb::VirtualDisplay { mode: pb::DisplayMode::Virtual as i32, width: 100, ..Default::default() })).unwrap();
        assert_eq!((w.private, w.width, w.height), (false, 1920, 1080));
    }

    #[test]
    fn xml_lists_requested_sizes_and_rate() {
        let x = settings_xml(&[(1916, 1042)], 144);
        assert!(x.contains("<width>1916</width>\n            <height>1042</height>"));
        assert!(x.contains("<width>3840</width>"));
        assert!(x.contains("<g_refresh_rate>144</g_refresh_rate>"));
        let x = settings_xml(&[], 59);
        assert!(x.contains("<g_refresh_rate>59</g_refresh_rate>"));
        assert_eq!(x.matches("<resolution>").count(), COMMON.len());
    }
}
