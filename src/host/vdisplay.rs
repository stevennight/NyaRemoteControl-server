//! Host display setup for a session: virtual screens (Virtual Display Driver,
//! optional component), physical displays on or off, local input blocked.
//!
//! The driver's device stays disabled while nobody uses it. A session that
//! asks for virtual screens gets it enabled with that many monitors at the
//! requested resolutions; the first one becomes the primary display. The
//! physical displays either stay on next to them or are switched off.
//! Dropping [`HostDisplays`] disables the device again and lets Windows
//! re-apply the layout it has stored for the physical monitors.
//!
//! The driver only offers the resolutions listed in its settings file, which
//! it reads when it starts: a size that is not listed yet (or a different
//! number of screens) means rewriting the file and restarting the device.

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
/// The driver fails to plug in its monitor ("monitor arrival", STATUS_NOT_SUPPORTED)
/// with around 100 or more modes; each resolution entry is one mode.
const MAX_MODES: usize = 64;
/// Session sizes kept in the list besides [`COMMON`] (newest last).
const MAX_EXTRA: usize = 8;
pub const MAX_SCREENS: usize = 4;

/// Turn on the driver's own log (`C:\VirtualDisplayDriver\Logs`), for `vdd-test --driver-log`.
static DRIVER_LOG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    /// Windows scaling in percent; 0 = leave as is.
    pub scale: u32,
}

/// What the client asked for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Setup {
    pub screens: Vec<Screen>,
    /// Physical displays off (only with at least one virtual screen).
    pub physical_off: bool,
    pub block_input: bool,
}

impl Setup {
    pub fn from_pb(v: Option<&pb::DisplaySetup>) -> Self {
        let Some(v) = v else { return Self::default() };
        let screens: Vec<Screen> = v
            .virtual_screens
            .iter()
            .take(MAX_SCREENS)
            .map(|s| {
                let (width, height) = if s.width >= 640 && s.height >= 480 {
                    (s.width.min(7680) & !1, s.height.min(4320) & !1)
                } else {
                    (1920, 1080)
                };
                Screen {
                    width,
                    height,
                    hz: if s.refresh_hz == 0 { 60 } else { s.refresh_hz.clamp(24, 240) },
                    scale: s.scale_percent.min(500),
                }
            })
            .collect();
        Self { physical_off: v.physical_off && !screens.is_empty(), screens, block_input: v.block_local_input }
    }

    /// Displays as they are: nothing to do.
    pub fn is_default(&self) -> bool {
        self.screens.is_empty() && !self.block_input
    }
}

/// Is the driver installed?
pub fn available() -> bool {
    devnode::exists(VDD_HWID)
}

/// The display setup of the attached session.
pub struct HostDisplays {
    setup: Setup,
    vdd: Option<Vdd>,
    /// The driver was enabled by us (also after a failed setup): release it.
    driver_on: bool,
    blocker: Option<InputBlocker>,
    /// Sizes requested during this session (kept in the driver's list).
    extra: Vec<(u32, u32)>,
}

/// The driver while it is up.
struct Vdd {
    luid: LUID,
    /// `\\.\DISPLAYn` of each virtual screen, in screen order.
    gdi_names: Vec<String>,
    /// Scaling applied per screen.
    scale_set: Vec<u32>,
}

impl HostDisplays {
    pub fn open(setup: Setup) -> Result<Self> {
        let mut d = Self { setup, vdd: None, driver_on: false, blocker: None, extra: Vec::new() };
        // On error, dropping `d` restores the physical displays.
        d.apply()?;
        Ok(d)
    }

    pub fn setup(&self) -> &Setup {
        &self.setup
    }

    /// GDI names of the virtual screens (the first is primary).
    pub fn gdi_names(&self) -> &[String] {
        self.vdd.as_ref().map(|v| v.gdi_names.as_slice()).unwrap_or(&[])
    }

    pub fn update(&mut self, setup: Setup) -> Result<()> {
        if setup == self.setup {
            return Ok(());
        }
        let restore_physical = self.setup.physical_off && !setup.physical_off;
        self.setup = setup;
        if restore_physical && self.driver_on {
            // Switched-off displays come back from Windows' stored layout;
            // then the virtual screens are added next to them again.
            self.release();
        }
        self.apply()
    }

    fn apply(&mut self) -> Result<()> {
        if self.setup.screens.is_empty() {
            self.release();
        } else {
            self.setup_vdd()?;
        }
        match (self.setup.block_input, self.blocker.is_some()) {
            (true, false) => self.blocker = Some(InputBlocker::start()),
            (false, true) => self.blocker = None,
            _ => {}
        }
        Ok(())
    }

    /// Bring the driver and the display layout to what `self.setup` says. Idempotent.
    fn setup_vdd(&mut self) -> Result<()> {
        let t0 = Instant::now();
        let screens = self.setup.screens.clone();
        let n = screens.len();
        let instance = devnode::instance_ids(VDD_HWID)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("被控端没有安装虚拟显示器驱动（在被控端管理界面的“可选组件”中安装）"))?;
        for s in &screens {
            let size = (s.width, s.height);
            if !COMMON.contains(&size) {
                self.extra.retain(|x| *x != size);
                self.extra.push(size);
                if self.extra.len() > MAX_EXTRA {
                    self.extra.remove(0);
                }
            }
        }
        let rates = rates(&screens);
        let changed = write_settings(n, &self.extra, &rates).context("写入虚拟显示器设置")?;
        if changed && devnode::is_started(VDD_HWID) {
            tracing::info!("virtual display: new settings ({n} screen(s)); restarting the driver");
            self.vdd = None;
            devnode::set_enabled(VDD_HWID, false).context("停用虚拟显示器")?;
            std::thread::sleep(Duration::from_millis(500));
        }
        // Adapters with displays before the driver starts: if its device path
        // does not match (it should), the new adapter is the driver's.
        let before = if devnode::is_started(VDD_HWID) { Vec::new() } else { target_adapters() };
        self.driver_on = true;
        devnode::set_enabled(VDD_HWID, true).context("启用虚拟显示器（需要管理员权限，开发模式下不可用）")?;
        let mut found = wait_for(Duration::from_secs(10), || find_vdd(&instance, &before));
        if found.is_none() {
            // Freshly installed or stuck: one restart of the device often helps.
            tracing::warn!("virtual display did not appear:\n{}restarting the device once", describe_state(&instance));
            let _ = devnode::set_enabled(VDD_HWID, false);
            std::thread::sleep(Duration::from_millis(1000));
            let before = target_adapters();
            devnode::set_enabled(VDD_HWID, true).context("启用虚拟显示器")?;
            found = wait_for(Duration::from_secs(10), || find_vdd(&instance, &before));
        }
        let Some(luid) = found else {
            let state = describe_state(&instance);
            tracing::error!("virtual display did not appear:\n{state}");
            return Err(anyhow!("虚拟显示器驱动已启用，但没有出现显示器。{}", summary(&state)));
        };
        // Every monitor of the driver arrives separately.
        if wait_for(Duration::from_secs(5), || (available_targets(luid) >= n).then_some(())).is_none() {
            tracing::warn!("virtual display: only {} of {n} screens appeared", available_targets(luid));
        }

        // Resolutions first, then the layout: the layout step saves the final
        // state, so later display changes don't bring an older one back.
        make_active(luid).context("启用虚拟显示器输出")?;
        let gdi_names = wait_for(Duration::from_secs(5), || {
            let names = vdd_gdi_names(luid);
            (names.len() >= n.min(available_targets(luid)).max(1)).then_some(names)
        })
        .ok_or_else(|| anyhow!("虚拟显示器没有进入桌面"))?;
        for (name, s) in gdi_names.iter().zip(&screens) {
            if let Err(e) = dc::set_mode(name, s.width, s.height, s.hz) {
                tracing::warn!("virtual display {name} mode {}x{}@{}: {e:#}", s.width, s.height, s.hz);
            }
        }
        layout(luid, self.setup.physical_off).context(if self.setup.physical_off { "关闭物理显示器" } else { "排列显示器" })?;
        let prev_scale = self.vdd.take().map(|v| v.scale_set).unwrap_or_default();
        let mut scale_set = vec![0; gdi_names.len()];
        for (i, (name, s)) in gdi_names.iter().zip(&screens).enumerate() {
            if s.scale == 0 || prev_scale.get(i) == Some(&s.scale) && !changed {
                scale_set[i] = prev_scale.get(i).copied().unwrap_or(0);
                continue;
            }
            match dc::set_scale(name, s.scale) {
                Ok(v) => {
                    scale_set[i] = s.scale;
                    tracing::info!("virtual display {name} scaling {v}%");
                }
                Err(e) => tracing::warn!("virtual display {name} scaling {}%: {e:#}", s.scale),
            }
        }
        // The capture side needs DXGI to see the new outputs.
        let first = gdi_names[0].clone();
        let _ = wait_for(Duration::from_secs(3), || {
            nya_win::topology::Topology::enumerate()
                .ok()
                .and_then(|t| t.outputs.iter().any(|o| o.device_name.eq_ignore_ascii_case(&first)).then_some(()))
        });
        tracing::info!(
            "virtual screens {:?} {:?}{} ready in {} ms",
            gdi_names,
            screens.iter().map(|s| format!("{}x{}@{}", s.width, s.height, s.hz)).collect::<Vec<_>>(),
            if self.setup.physical_off { ", physical displays off" } else { "" },
            t0.elapsed().as_millis()
        );
        self.vdd = Some(Vdd { luid, gdi_names, scale_set });
        Ok(())
    }

    /// After a display change (hot-plug, Windows restoring a layout): make
    /// sure the physical displays are still off when they should be.
    /// Returns whether something had to be changed.
    pub fn enforce(&mut self) -> Result<bool> {
        let Some(v) = &self.vdd else { return Ok(false) };
        if !self.setup.physical_off {
            return Ok(false);
        }
        let cfg = Config::query(false)?;
        let others = cfg.active().filter(|p| !luid_eq(p.targetInfo.adapterId, v.luid)).count();
        if others == 0 {
            return Ok(false);
        }
        layout(v.luid, true)?;
        Ok(true)
    }
}

impl HostDisplays {
    fn release(&mut self) {
        self.vdd = None;
        if std::mem::take(&mut self.driver_on) {
            release();
        }
    }
}

impl Drop for HostDisplays {
    fn drop(&mut self) {
        self.blocker = None;
        self.release();
    }
}

/// Disable the driver's device and restore the physical layout.
fn release() {
    match devnode::set_enabled(VDD_HWID, false) {
        Ok(_) => tracing::info!("virtual display removed"),
        Err(e) => tracing::warn!("disable virtual display: {e:#}"),
    }
    // Windows usually re-applies the stored layout by itself; make sure, also
    // when the virtual screens were the only active displays.
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

/// Adapters that have an available (connected) display target.
fn target_adapters() -> Vec<LUID> {
    let mut v: Vec<LUID> = Vec::new();
    if let Ok(cfg) = Config::query(true) {
        for p in cfg.paths.iter().filter(|p| p.targetInfo.targetAvailable.as_bool()) {
            if !v.iter().any(|s| luid_eq(*s, p.targetInfo.adapterId)) {
                v.push(p.targetInfo.adapterId);
            }
        }
    }
    v
}

/// Connected monitors of an adapter.
fn available_targets(luid: LUID) -> usize {
    let Ok(cfg) = Config::query(true) else { return 0 };
    let mut ids: Vec<u32> = cfg
        .paths
        .iter()
        .filter(|p| luid_eq(p.targetInfo.adapterId, luid) && p.targetInfo.targetAvailable.as_bool())
        .map(|p| p.targetInfo.id)
        .collect();
    ids.sort();
    ids.dedup();
    ids.len()
}

/// Adapter LUID of the driver's device (the indirect display adapter), once
/// it has a display: matched by device path, else the one adapter that got
/// a display since `before`.
fn find_vdd(instance: &str, before: &[LUID]) -> Option<LUID> {
    let now = target_adapters();
    if let Some(l) = now.iter().find(|&&l| dc::adapter_path(l).is_some_and(|p| dc::adapter_path_matches(&p, instance))) {
        return Some(*l);
    }
    if before.is_empty() {
        return None;
    }
    let new: Vec<LUID> = now.into_iter().filter(|l| !before.iter().any(|b| luid_eq(*b, *l))).collect();
    if new.len() == 1 {
        tracing::warn!(
            "virtual display adapter found as the new adapter (path {:?} does not match {instance})",
            dc::adapter_path(new[0])
        );
        return Some(new[0]);
    }
    None
}

/// Active paths of the driver, in monitor order.
fn vdd_paths(cfg: &Config, luid: LUID) -> Vec<DISPLAYCONFIG_PATH_INFO> {
    let mut v: Vec<DISPLAYCONFIG_PATH_INFO> = cfg.active().filter(|p| luid_eq(p.targetInfo.adapterId, luid)).copied().collect();
    v.sort_by_key(|p| p.targetInfo.id);
    v
}

fn vdd_gdi_names(luid: LUID) -> Vec<String> {
    let Ok(cfg) = Config::query(false) else { return Vec::new() };
    vdd_paths(&cfg, luid).iter().filter_map(dc::source_gdi_name).collect()
}

/// Make every connected monitor of the driver active (next to whatever is active).
fn make_active(luid: LUID) -> Result<()> {
    let all = Config::query(true)?;
    let on_vdd = |p: &DISPLAYCONFIG_PATH_INFO| luid_eq(p.targetInfo.adapterId, luid);
    let mut paths: Vec<_> = all.active().copied().collect();
    let mut added = 0;
    for cand in all.paths.iter().filter(|p| on_vdd(p) && p.targetInfo.targetAvailable.as_bool()) {
        let target_active = paths.iter().any(|p| on_vdd(p) && p.targetInfo.id == cand.targetInfo.id);
        let source_used = paths.iter().any(|p| luid_eq(p.sourceInfo.adapterId, cand.sourceInfo.adapterId) && p.sourceInfo.id == cand.sourceInfo.id);
        if target_active || source_used {
            continue;
        }
        let mut p = *cand;
        p.flags |= PATH_ACTIVE;
        p.sourceInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
        p.targetInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
        paths.push(p);
        added += 1;
    }
    if added == 0 {
        return Ok(());
    }
    tracing::info!("display layout: activating {added} virtual screen(s)");
    dc::apply(&paths, &all.modes, false)
}

/// Virtual screens side by side from (0, 0) (the first is primary); the
/// physical displays switched off, or to the right in their own arrangement.
/// Saved, so Windows keeps it for this set of monitors.
fn layout(luid: LUID, physical_off: bool) -> Result<()> {
    for attempt in 0..3 {
        let cfg = Config::query(false)?;
        let mine = vdd_paths(&cfg, luid);
        if mine.is_empty() {
            anyhow::bail!("virtual screens not active");
        }
        let others: Vec<DISPLAYCONFIG_PATH_INFO> = cfg.active().filter(|p| !luid_eq(p.targetInfo.adapterId, luid)).copied().collect();
        let mut modes = cfg.modes.clone();
        let (vdd_ok, x_end) = place(&mine, &mut modes, 0);
        let mut paths = mine.clone();
        let mut others_ok = true;
        if physical_off {
            others_ok = others.is_empty();
        } else {
            others_ok &= shift_right(&others, &mut modes, x_end);
            paths.extend(others.iter().copied());
        }
        if vdd_ok && others_ok {
            return Ok(());
        }
        tracing::info!(
            "display layout: {} virtual screen(s) from (0,0){}",
            mine.len(),
            if physical_off { format!(", {} physical display(s) off", others.len()) } else { ", physical displays to the right".into() }
        );
        dc::apply(&paths, &modes, true)?;
        std::thread::sleep(Duration::from_millis(300 * (attempt + 1)));
    }
    let cfg = Config::query(false)?;
    if physical_off && cfg.active().any(|p| !luid_eq(p.targetInfo.adapterId, luid)) {
        anyhow::bail!("物理显示器关不掉（Windows 一直把它重新打开）");
    }
    Ok(())
}

fn source_idx(p: &DISPLAYCONFIG_PATH_INFO) -> usize {
    unsafe { p.sourceInfo.Anonymous.modeInfoIdx as usize }
}

/// Put these sources in a row starting at `x0`, y = 0. Returns whether they
/// already were there, and the x after the last one.
fn place(paths: &[DISPLAYCONFIG_PATH_INFO], modes: &mut [DISPLAYCONFIG_MODE_INFO], x0: i32) -> (bool, i32) {
    let (mut ok, mut x) = (true, x0);
    let mut done: Vec<usize> = Vec::new();
    for p in paths {
        let i = source_idx(p);
        if i >= modes.len() || done.contains(&i) {
            continue;
        }
        done.push(i);
        let m = unsafe { &mut modes[i].Anonymous.sourceMode };
        if m.position.x != x || m.position.y != 0 {
            ok = false;
            m.position.x = x;
            m.position.y = 0;
        }
        x += m.width as i32;
    }
    (ok, x)
}

/// Move these sources (keeping their arrangement) so they start at `x0`, top 0.
fn shift_right(paths: &[DISPLAYCONFIG_PATH_INFO], modes: &mut [DISPLAYCONFIG_MODE_INFO], x0: i32) -> bool {
    let mut idx: Vec<usize> = paths.iter().map(source_idx).filter(|&i| i < modes.len()).collect();
    idx.sort();
    idx.dedup();
    if idx.is_empty() {
        return true;
    }
    let pos = |m: &DISPLAYCONFIG_MODE_INFO| unsafe { m.Anonymous.sourceMode.position };
    let min_x = idx.iter().map(|&i| pos(&modes[i]).x).min().unwrap();
    let min_y = idx.iter().map(|&i| pos(&modes[i]).y).min().unwrap();
    if min_x == x0 && min_y == 0 {
        return true;
    }
    for &i in &idx {
        let p = unsafe { &mut modes[i].Anonymous.sourceMode.position };
        p.x = p.x - min_x + x0;
        p.y -= min_y;
    }
    false
}

/// Device and display-path state, for the log and `vdd-test`.
pub fn describe_state(instance: &str) -> String {
    let mut s = String::new();
    for d in devnode::states(VDD_HWID) {
        s += &format!(
            "设备 {}：{}，{}\n",
            d.instance_id,
            if d.started { "已启动" } else { "未启动" },
            devnode::problem_text(d.problem)
        );
    }
    match Config::query(true) {
        Ok(cfg) => {
            let mut seen: Vec<LUID> = Vec::new();
            for p in &cfg.paths {
                let l = p.targetInfo.adapterId;
                if seen.iter().any(|x| luid_eq(*x, l)) {
                    continue;
                }
                seen.push(l);
                let targets = cfg.paths.iter().filter(|q| luid_eq(q.targetInfo.adapterId, l));
                let (mut n, mut avail, mut active) = (0, 0, 0);
                let mut ids: Vec<u32> = Vec::new();
                for q in targets {
                    if !ids.contains(&q.targetInfo.id) {
                        ids.push(q.targetInfo.id);
                        n += 1;
                        if q.targetInfo.targetAvailable.as_bool() {
                            avail += 1;
                        }
                    }
                    if q.flags & PATH_ACTIVE != 0 {
                        active += 1;
                    }
                }
                let path = dc::adapter_path(l).unwrap_or_else(|| "?".into());
                let mark = if dc::adapter_path_matches(&path, instance) { "  <- 虚拟显示器" } else { "" };
                s += &format!("显卡 {path}：输出 {n} 个，已连接 {avail} 个，使用中 {active} 个{mark}\n");
            }
        }
        Err(e) => s += &format!("QueryDisplayConfig 失败：{e:#}\n"),
    }
    let settings = Path::new(VDD_SETTINGS_DIR).join("vdd_settings.xml");
    s += &format!("设置文件 {}：{}\n", settings.display(), if settings.exists() { "存在" } else { "不存在" });
    s
}

/// Print the end of the driver's newest log file.
fn print_driver_log() {
    let dir = Path::new(VDD_SETTINGS_DIR).join("Logs");
    let newest = std::fs::read_dir(&dir)
        .ok()
        .and_then(|rd| rd.flatten().max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok()));
    match newest {
        Some(e) => {
            let text = std::fs::read(e.path()).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            println!("== 驱动日志 {}（最后 {} 行）", e.path().display(), lines.len().min(80));
            for l in &lines[lines.len().saturating_sub(80)..] {
                println!("{l}");
            }
        }
        None => println!("== 驱动日志：{} 下没有日志文件（驱动没有读到设置，或者没有写入权限）", dir.display()),
    }
}

/// The most useful hint from [`describe_state`] for the client.
fn summary(state: &str) -> String {
    let dev = devnode::states(VDD_HWID);
    if dev.iter().any(|d| d.problem == 14) {
        return "被控端需要重启电脑后才能使用虚拟显示器".into();
    }
    if let Some(d) = dev.iter().find(|d| !d.started) {
        return format!("驱动状态：{}", devnode::problem_text(d.problem));
    }
    if state.contains("<- 虚拟显示器") {
        "驱动已运行但没有创建显示器，请在被控端运行 nya-server-svc vdd-test --driver-log 查看详情".into()
    } else {
        "没有找到驱动对应的显卡，请在被控端运行 nya-server-svc vdd-test 查看详情".into()
    }
}

/// `nya-server-svc vdd-test`: create virtual screens, show what happened,
/// keep them for `hold`, then remove them.
pub fn self_test(screens: usize, physical_off: bool, hold: Duration, driver_log: bool) -> Result<()> {
    if !crate::winutil::is_elevated() {
        anyhow::bail!("需要管理员权限：请右键“以管理员身份运行”终端后再执行");
    }
    DRIVER_LOG.store(driver_log, std::sync::atomic::Ordering::Relaxed);
    let instance = devnode::instance_ids(VDD_HWID).into_iter().next().unwrap_or_default();
    println!("== 启用前\n{}", describe_state(&instance));
    let screen = Screen { width: 1920, height: 1080, hz: 60, scale: 0 };
    let setup = Setup { screens: vec![screen; screens.clamp(1, MAX_SCREENS)], physical_off, block_input: false };
    let res = HostDisplays::open(setup);
    println!("== 启用后\n{}", describe_state(&instance));
    if driver_log {
        print_driver_log();
    }
    let d = res?;
    println!("虚拟显示器 {:?} 已创建，{} 秒后移除…", d.gdi_names(), hold.as_secs());
    if let Ok(t) = nya_win::topology::Topology::enumerate() {
        for o in &t.outputs {
            println!("  DXGI 输出 {} {}x{} @({},{}) 主={}", o.device_name, o.width(), o.height(), o.left, o.top, o.primary);
        }
    }
    std::thread::sleep(hold);
    drop(d);
    println!("已移除，显示器布局已恢复");
    Ok(())
}

/// Refresh rates offered for every resolution: 60 Hz and what the screens use.
fn rates(screens: &[Screen]) -> Vec<u32> {
    let mut v = vec![60];
    for s in screens {
        if !v.contains(&s.hz) && v.len() < 3 {
            v.push(s.hz);
        }
    }
    v
}

/// The driver's settings. No global refresh rates: the driver multiplies them
/// with every resolution (on top of each resolution's own rate), which
/// quickly exceeds what it can report. Instead one entry per resolution and
/// rate, at most [`MAX_MODES`].
fn settings_xml(count: usize, sizes: &[(u32, u32)], rates: &[u32]) -> String {
    let mut x = format!(
        "<?xml version='1.0' encoding='utf-8'?>\n<!-- Written by NyaRemoteControl for each session; edits are overwritten. -->\n<vdd_settings>\n    <monitors>\n        <count>{count}</count>\n    </monitors>\n    <gpu>\n        <friendlyname>default</friendlyname>\n    </gpu>\n    <global>\n    </global>\n    <resolutions>\n"
    );
    // Session sizes first, so they survive the cap.
    let mut n = 0;
    'outer: for (w, h) in sizes.iter().rev().chain(COMMON.iter()) {
        for r in rates {
            if n >= MAX_MODES {
                break 'outer;
            }
            n += 1;
            x += &format!(
                "        <resolution>\n            <width>{w}</width>\n            <height>{h}</height>\n            <refresh_rate>{r}</refresh_rate>\n        </resolution>\n"
            );
        }
    }
    let log = DRIVER_LOG.load(std::sync::atomic::Ordering::Relaxed);
    x += &format!(
        "    </resolutions>\n    <options>\n        <CustomEdid>false</CustomEdid>\n        <PreventSpoof>false</PreventSpoof>\n        <EdidCeaOverride>false</EdidCeaOverride>\n        <HardwareCursor>true</HardwareCursor>\n        <SDR10bit>false</SDR10bit>\n        <HDRPlus>false</HDRPlus>\n        <logging>{log}</logging>\n        <debuglogging>{log}</debuglogging>\n    </options>\n</vdd_settings>\n"
    );
    x
}

/// Write the driver's settings; true if the file changed while the driver runs.
fn write_settings(count: usize, sizes: &[(u32, u32)], rates: &[u32]) -> Result<bool> {
    let path = Path::new(VDD_SETTINGS_DIR).join("vdd_settings.xml");
    let xml = settings_xml(count, sizes, rates);
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

    fn screen(w: u32, h: u32) -> pb::VirtualScreen {
        pb::VirtualScreen { width: w, height: h, ..Default::default() }
    }

    #[test]
    fn setup_from_request() {
        assert!(Setup::from_pb(None).is_default());
        assert!(Setup::from_pb(Some(&pb::DisplaySetup::default())).is_default());
        // Physical displays can't be switched off without a virtual screen.
        let s = Setup::from_pb(Some(&pb::DisplaySetup { physical_off: true, ..Default::default() }));
        assert!(s.is_default() && !s.physical_off);
        let s = Setup::from_pb(Some(&pb::DisplaySetup {
            virtual_screens: vec![
                pb::VirtualScreen { width: 1917, height: 1043, refresh_hz: 0, scale_percent: 150 },
                screen(100, 100),
            ],
            physical_off: true,
            block_local_input: true,
        }));
        assert_eq!(s.screens[0], Screen { width: 1916, height: 1042, hz: 60, scale: 150 });
        assert_eq!((s.screens[1].width, s.screens[1].height), (1920, 1080));
        assert!(s.physical_off && s.block_input);
        let many = Setup::from_pb(Some(&pb::DisplaySetup { virtual_screens: vec![screen(0, 0); 9], ..Default::default() }));
        assert_eq!(many.screens.len(), MAX_SCREENS);
        // Blocking input alone is a real setup (no virtual screen).
        let b = Setup::from_pb(Some(&pb::DisplaySetup { block_local_input: true, ..Default::default() }));
        assert!(!b.is_default() && b.screens.is_empty());
    }

    #[test]
    fn xml_lists_count_sizes_and_rates() {
        let x = settings_xml(2, &[(1916, 1042)], &[60, 144]);
        assert!(x.contains("<count>2</count>"));
        assert!(x.contains("<width>1916</width>\n            <height>1042</height>\n            <refresh_rate>60</refresh_rate>"));
        assert!(x.contains("<width>1916</width>\n            <height>1042</height>\n            <refresh_rate>144</refresh_rate>"));
        assert!(x.contains("<width>3840</width>"));
        assert!(!x.contains("<g_refresh_rate>"));
        assert_eq!(x.matches("<resolution>").count(), (COMMON.len() + 1) * 2);
        let x = settings_xml(1, &[], &[60]);
        assert_eq!(x.matches("<resolution>").count(), COMMON.len());
    }

    #[test]
    fn rates_are_few() {
        let s = |hz| Screen { width: 1920, height: 1080, hz, scale: 0 };
        assert_eq!(rates(&[s(60)]), vec![60]);
        assert_eq!(rates(&[s(144), s(144)]), vec![60, 144]);
        assert_eq!(rates(&[s(144), s(165), s(240)]), vec![60, 144, 165]);
    }

    /// Worst case stays under the driver's limit (it failed at 108, worked at 96).
    #[test]
    fn mode_count_is_capped() {
        let extra: Vec<(u32, u32)> = (0..MAX_EXTRA as u32).map(|i| (1000 + 8 * i, 700)).collect();
        let x = settings_xml(4, &extra, &[60, 144, 165]);
        assert!(x.matches("<resolution>").count() <= MAX_MODES);
        // Session sizes are never the ones cut.
        assert!(x.contains(&format!("<width>{}</width>", 1000 + 8 * (MAX_EXTRA as u32 - 1))));
        assert!(x.contains("<width>1000</width>"));
    }
}
