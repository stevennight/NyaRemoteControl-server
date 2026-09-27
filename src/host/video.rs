//! Video thread: owns the GPU topology, probes encoders, builds/rebuilds the
//! pipeline and drives it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use nya_proto::pb;
use nya_win::desktop::DesktopTracker;
use nya_win::topology::Topology;

use super::input::InputCmd;
use super::pipeline::{Pipeline, Step};
use super::select::{self, EncoderProbe, Plan};
use super::{HostConfig, Sink};
use crate::ipc_pb::host_event::Ev;

pub enum VideoCmd {
    Start(pb::StartStream),
    Stop,
    Keyframe,
    SetMode(pb::StreamMode),
    Caps(pb::ClientCaps),
    FrameSent(u64),
    Shutdown,
}

struct State {
    topo: Topology,
    probes: Vec<EncoderProbe>,
    req: Option<pb::StartStream>,
    caps: Option<pb::ClientCaps>,
    pipe: Option<Pipeline>,
    retry_at: Option<Instant>,
    frame_counter: u64,
    next_stream_id: u64,
    /// Plans that opened fine but failed while encoding; skipped after two failures.
    failed: HashMap<Plan, u32>,
}

pub fn session_info(topo: &Topology, probes: &[EncoderProbe], cfg: &HostConfig) -> pb::SessionInfo {
    pb::SessionInfo {
        host_name: cfg.name.clone(),
        displays: display_infos(topo),
        gpus: topo
            .adapters
            .iter()
            .map(|a| {
                let probe = probes.iter().find(|p| p.adapter_index == a.index);
                pb::GpuInfo {
                    index: a.index,
                    name: a.name.clone(),
                    vendor_id: a.vendor_id,
                    luid: a.luid,
                    encoders: probe
                        .map(|p| {
                            p.caps
                                .iter()
                                .map(|&(c, yuv444)| pb::CodecCap {
                                    codec: select::to_pb_codec(c) as i32,
                                    chroma: if yuv444 { pb::Chroma::Yuv444 } else { pb::Chroma::Yuv420 } as i32,
                                    hardware: true,
                                    ..Default::default()
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    encoder_backend: probe.map(|p| p.backend.name().to_owned()).unwrap_or_default(),
                }
            })
            .collect(),
    }
}

fn display_infos(topo: &Topology) -> Vec<pb::DisplayInfo> {
    topo.outputs
        .iter()
        .map(|o| pb::DisplayInfo {
            id: o.id,
            name: o.device_name.clone(),
            width: o.width(),
            height: o.height(),
            x: o.left,
            y: o.top,
            refresh_hz: o.refresh_hz,
            primary: o.primary,
            gpu_index: o.adapter_index,
        })
        .collect()
}

fn luids(t: &Topology) -> Vec<u64> {
    let mut v: Vec<u64> = t.adapters.iter().map(|a| a.luid).collect();
    v.sort();
    v
}

pub fn thread(rx: Receiver<VideoCmd>, sink: Sink, input_tx: Sender<InputCmd>, cfg: HostConfig) {
    nya_win::com_init();
    nya_win::mmcss_boost("Capture");
    let mut desktop = DesktopTracker::new();
    if let Err(e) = desktop.sync() {
        tracing::warn!("cannot attach to input desktop: {e:#}");
    }
    let topo = loop {
        match Topology::enumerate() {
            Ok(t) => break t,
            Err(e) => {
                tracing::error!("GPU enumeration failed: {e:#}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    let probes = select::probe_all(&topo);
    sink.send(Ev::SessionInfo(session_info(&topo, &probes, &cfg)));

    let mut st = State {
        topo,
        probes,
        req: None,
        caps: None,
        pipe: None,
        retry_at: None,
        frame_counter: 0,
        next_stream_id: nya_proto::now_us(),
        failed: HashMap::new(),
    };
    let mut last_topo_check = Instant::now();

    loop {
        // --- commands ---
        let first = if st.pipe.is_some() {
            rx.try_recv().ok()
        } else if let Some(at) = st.retry_at {
            match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        let mut rebuild = false;
        for cmd in first.into_iter().chain(rx.try_iter()) {
            match cmd {
                VideoCmd::Start(s) => {
                    st.req = Some(s);
                    rebuild = true;
                }
                VideoCmd::Stop => {
                    drop_pipe(&mut st);
                    st.req = None;
                    st.retry_at = None;
                }
                VideoCmd::Keyframe => {
                    if let Some(p) = st.pipe.as_mut() {
                        p.request_keyframe();
                    }
                }
                VideoCmd::SetMode(m) => {
                    if let Some(r) = st.req.as_mut() {
                        let c = r.config.get_or_insert_with(Default::default);
                        if c.mode != m as i32 {
                            c.mode = m as i32;
                            // Let the server pick codec/chroma/bitrate/fps for the new mode.
                            c.chroma = 0;
                            c.fps = 0;
                            c.bitrate_kbps = 0;
                            rebuild = true;
                        }
                    }
                }
                VideoCmd::Caps(c) => st.caps = Some(c),
                VideoCmd::FrameSent(id) => {
                    if let Some(p) = st.pipe.as_mut() {
                        p.frame_sent(id);
                    }
                }
                VideoCmd::Shutdown => return,
            }
        }

        // --- topology changes (hot-plug, MUX switch, driver reset) ---
        if last_topo_check.elapsed() > Duration::from_secs(1) {
            last_topo_check = Instant::now();
            if !st.topo.is_current() {
                refresh_topology(&mut st, &sink, &cfg);
                if st.pipe.is_some() {
                    rebuild = true;
                }
            }
        }

        // --- (re)build ---
        let retry_due = st.retry_at.is_some_and(|t| Instant::now() >= t);
        if st.req.is_some() && (rebuild || (st.pipe.is_none() && (retry_due || st.retry_at.is_none()))) {
            drop_pipe(&mut st);
            match build(&mut st, &cfg, &mut desktop) {
                Ok(p) => {
                    sink.send(Ev::StreamStarted(p.started.clone()));
                    let _ = input_tx.send(InputCmd::SetRect(p.rect));
                    st.pipe = Some(p);
                    st.retry_at = None;
                }
                Err(msg) => {
                    tracing::error!("cannot start stream: {msg}");
                    sink.send(Ev::StreamError(pb::StreamError { message: msg }));
                    st.retry_at = Some(Instant::now() + Duration::from_secs(3));
                }
            }
        }

        // --- run ---
        if let Some(p) = st.pipe.as_mut() {
            if let Step::Rebuild(why) = p.step(&sink, &mut desktop) {
                tracing::warn!("rebuilding stream: {why}");
                if why.starts_with("encode failed") {
                    *st.failed.entry(p.plan.clone()).or_default() += 1;
                }
                drop_pipe(&mut st);
                refresh_topology(&mut st, &sink, &cfg);
                st.retry_at = Some(Instant::now() + Duration::from_millis(300));
            }
        }
    }
}

fn drop_pipe(st: &mut State) {
    if let Some(p) = st.pipe.take() {
        st.frame_counter = st.frame_counter.max(p.frame_id);
    }
}

fn refresh_topology(st: &mut State, sink: &Sink, cfg: &HostConfig) {
    match Topology::enumerate() {
        Ok(t) => {
            if luids(&t) != luids(&st.topo) {
                tracing::info!("GPU set changed; re-probing encoders");
                st.probes = select::probe_all(&t);
            }
            st.topo = t;
            st.failed.clear();
            sink.send(Ev::DisplayChanged(pb::DisplayChanged { displays: display_infos(&st.topo) }));
            sink.send(Ev::SessionInfo(session_info(&st.topo, &st.probes, cfg)));
        }
        Err(e) => tracing::warn!("GPU enumeration failed: {e:#}"),
    }
}

fn build(st: &mut State, cfg: &HostConfig, desktop: &mut DesktopTracker) -> Result<Pipeline, String> {
    let req = st.req.clone().unwrap();
    let output = st
        .topo
        .output(req.display_id)
        .or_else(|| st.topo.outputs.first())
        .ok_or_else(|| "没有可用的显示器".to_string())?
        .clone();
    let plans = select::plans(&st.probes, output.adapter_index, &req, st.caps.as_ref(), &cfg.encoder);
    let mut errors = Vec::new();
    for plan in plans {
        if st.failed.get(&plan).copied().unwrap_or(0) >= 2 {
            tracing::info!("skipping plan that failed while encoding: {plan:?}");
            continue;
        }
        st.next_stream_id += 1;
        match Pipeline::build(
            &st.topo,
            &output,
            &plan,
            &req,
            st.caps.as_ref(),
            cfg,
            st.next_stream_id,
            st.frame_counter,
            desktop,
        ) {
            Ok(p) => {
                if !errors.is_empty() {
                    tracing::info!("fell back to {:?} {:?} after {} failures", p.backend(), plan.codec, errors.len());
                }
                return Ok(p);
            }
            Err(e) => {
                tracing::warn!("plan {plan:?} failed: {e:#}");
                errors.push(format!("{:?}/{:?}: {e:#}", plan.backend, plan.codec));
            }
        }
    }
    Err(if errors.is_empty() { "客户端不支持任何可用的编码格式".into() } else { errors.join("; ") })
}
