//! Connection, handshake, pairing, and the long-running session with
//! automatic reconnection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::Sender;
use nya_proto::frame::{datagram_type, stream_type, AudioPacket, VideoFrameHeader};
use nya_proto::framing::{encode_varint, expect_msg, read_msg, read_varint, write_msg};
use nya_proto::negotiate::{self, LocalVersion, Negotiated};
use nya_proto::pb::{self, control_msg::Msg, Feature};
use nya_proto::{MAX_MESSAGE_LEN, MAX_VIDEO_FRAME_LEN};
use nya_transport::identity::peer_fingerprint;
use nya_transport::pairing::{self, PairingKey, Transcript};
use nya_transport::quinn::{Connection, Endpoint, RecvStream, SendStream};
use nya_transport::{Fingerprint, Identity};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::events::{NetCmd, Ui, UiEvent};
use crate::stats::Shared;
use crate::video::VideoIn;

pub type PairPrompt = Arc<dyn Fn() -> Option<String> + Send + Sync>;

pub struct Link {
    pub endpoint: Endpoint,
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub neg: Negotiated,
    pub welcome: pb::Welcome,
    pub server_fp: Fingerprint,
}

fn ctl(m: Msg) -> pb::ControlMsg {
    pb::ControlMsg { msg: Some(m) }
}

/// Connect and complete the handshake (and pairing if the host asks for it).
pub async fn connect(
    addr: SocketAddr,
    id: &Identity,
    pinned: Option<Fingerprint>,
    client_name: &str,
    prompt: Option<PairPrompt>,
) -> Result<Link> {
    let endpoint = nya_transport::endpoint::client_endpoint(addr)?;
    let conn = timeout(Duration::from_secs(8), nya_transport::endpoint::connect(&endpoint, addr, id, pinned))
        .await
        .map_err(|_| anyhow!("连接 {addr} 超时（检查组网是否连通、被控端是否运行、防火墙 UDP 端口）"))??;
    let server_fp = peer_fingerprint(&conn).ok_or_else(|| anyhow!("被控端没有证书"))?;
    let (mut send, mut recv) = conn.open_bi().await?;

    let me = LocalVersion::current();
    write_msg(&mut send, &negotiate::hello(&me, client_name, env!("CARGO_PKG_VERSION"))).await?;
    let reply: pb::HelloReply = timeout(Duration::from_secs(10), expect_msg(&mut recv, MAX_MESSAGE_LEN))
        .await
        .context("等待被控端响应超时")??;
    let welcome = match reply.reply {
        Some(pb::hello_reply::Reply::Welcome(w)) => w,
        Some(pb::hello_reply::Reply::Reject(r)) => bail!("被控端拒绝连接：{}", r.message),
        None => bail!("被控端响应无法识别（版本差异过大？）"),
    };
    let neg = negotiate::accept_welcome(&welcome, &me).map_err(|e| anyhow!(e))?;

    if welcome.needs_pairing {
        let challenge: pb::ControlMsg = expect_msg(&mut recv, MAX_MESSAGE_LEN).await?;
        let Some(Msg::AuthChallenge(ch)) = challenge.msg else { bail!("expected AuthChallenge") };
        let Some(prompt) = prompt else {
            bail!("被控端要求重新配对（可能被控端重装或移除了本机），请重新运行并输入配对码");
        };
        let code = tokio::task::spawn_blocking(move || prompt()).await?.ok_or_else(|| anyhow!("已取消配对"))?;
        let key = PairingKey::from_code(&code).ok_or_else(|| anyhow!("配对码格式不正确"))?;
        let client_nonce = pairing::nonce();
        let t = Transcript {
            server_nonce: &ch.server_nonce,
            client_nonce: &client_nonce,
            server_fp,
            client_fp: id.fingerprint(),
        };
        write_msg(&mut send, &ctl(Msg::AuthResponse(pb::AuthResponse { client_nonce: client_nonce.to_vec(), mac: t.client_mac(&key) })))
            .await?;
        let res: pb::ControlMsg = expect_msg(&mut recv, MAX_MESSAGE_LEN).await?;
        let Some(Msg::AuthResult(r)) = res.msg else { bail!("expected AuthResult") };
        if !r.ok {
            bail!("配对失败：{}", r.message);
        }
        if !t.verify_server(&key, &r.server_mac) {
            bail!("被控端无法证明它知道配对码，已中止（可能存在中间人）");
        }
    } else if pinned.is_none() {
        tracing::warn!("被控端已认识本机但本机没有保存它的指纹；将信任并保存 {server_fp}");
    }
    Ok(Link { endpoint, conn, send, recv, neg, welcome, server_fp })
}

/// Everything needed to (re)start the session.
/// Decoder input of every open window, by stream slot.
#[derive(Default)]
pub struct VideoRoutes(std::sync::Mutex<std::collections::HashMap<u32, Sender<VideoIn>>>);

impl VideoRoutes {
    pub fn set(&self, slot: u32, tx: Option<Sender<VideoIn>>) {
        let mut m = self.0.lock().unwrap();
        match tx {
            Some(tx) => {
                m.insert(slot, tx);
            }
            None => {
                m.remove(&slot);
            }
        }
    }

    fn get(&self, slot: u32) -> Option<Sender<VideoIn>> {
        self.0.lock().unwrap().get(&slot).cloned()
    }

    /// Reconnect: every decoder waits for a new keyframe.
    fn reset_all(&self) {
        for tx in self.0.lock().unwrap().values() {
            let _ = tx.send(VideoIn::Reset);
        }
    }
}

pub struct Params {
    pub addr: SocketAddr,
    pub pinned: Fingerprint,
    pub identity: Identity,
    pub name: String,
    pub caps: pb::ClientCaps,
    pub start: pb::StartStream,
    /// Streams of extra windows (slot > 0), replayed after a reconnect.
    pub extra: std::collections::BTreeMap<u32, pb::StartStream>,
    /// Folders shown on the host as a drive (FEATURE_FOLDER_MOUNT).
    pub shares: Arc<nya_transport::folders::Shares>,
}

pub struct Sinks {
    pub ui: Ui,
    pub video: Arc<VideoRoutes>,
    pub audio: Sender<AudioPacket>,
    pub stats: Arc<Shared>,
    pub clip: Arc<crate::transfer::ClipFiles>,
}

enum End {
    UserQuit,
    Fatal(String),
    Lost(String),
}

/// Run the session; reconnect for up to two minutes when the link drops.
pub async fn supervise(first: Link, mut p: Params, mut cmds: mpsc::UnboundedReceiver<NetCmd>, sinks: Sinks) {
    let mut link = Some(first);
    let mut lost_since: Option<Instant> = None;
    loop {
        let l = match link.take() {
            Some(l) => l,
            None => match connect(p.addr, &p.identity, Some(p.pinned), &p.name, None).await {
                Ok(l) => l,
                Err(e) => {
                    let since = *lost_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_secs(120) {
                        sinks.ui.send(UiEvent::Disconnected(format!("无法重新连接：{e:#}")));
                        return;
                    }
                    sinks.ui.send(UiEvent::Reconnecting(format!("{e:#}")));
                    // Drain commands meanwhile; honour Quit.
                    let deadline = tokio::time::sleep(Duration::from_secs(2));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => break,
                            c = cmds.recv() => match c {
                                Some(NetCmd::Quit) | None => return,
                                Some(NetCmd::Control(m)) => track(&mut p, &m),
                                _ => {}
                            }
                        }
                    }
                    continue;
                }
            },
        };
        lost_since = None;
        sinks.video.reset_all();
        match run(l, &mut p, &mut cmds, &sinks).await {
            End::UserQuit => return,
            End::Fatal(msg) => {
                sinks.ui.send(UiEvent::Disconnected(msg));
                return;
            }
            End::Lost(msg) => {
                tracing::warn!("connection lost: {msg}");
                sinks.ui.send(UiEvent::Reconnecting(msg));
            }
        }
    }
}

/// Keep the replayable state current.
fn track(p: &mut Params, m: &pb::ControlMsg) {
    match &m.msg {
        Some(Msg::StartStream(s)) if s.slot != 0 => {
            p.extra.insert(s.slot, s.clone());
        }
        Some(Msg::StopStream(s)) if s.slot != 0 => {
            p.extra.remove(&s.slot);
        }
        Some(Msg::StartStream(s)) => p.start = s.clone(),
        Some(Msg::SetMode(m)) => {
            p.start.config.get_or_insert_with(Default::default).mode = m.mode;
            for s in p.extra.values_mut() {
                s.config.get_or_insert_with(Default::default).mode = m.mode;
            }
        }
        Some(Msg::ClientCaps(c)) => p.caps = c.clone(),
        _ => {}
    }
}

async fn run(link: Link, p: &mut Params, cmds: &mut mpsc::UnboundedReceiver<NetCmd>, sinks: &Sinks) -> End {
    let Link { endpoint: _endpoint, conn, mut send, mut recv, neg, .. } = link;
    sinks.ui.send(UiEvent::Connected);

    let setup = async {
        write_msg(&mut send, &ctl(Msg::ClientCaps(p.caps.clone()))).await?;
        write_msg(&mut send, &ctl(Msg::StartStream(p.start.clone()))).await?;
        for s in p.extra.values() {
            write_msg(&mut send, &ctl(Msg::StartStream(s.clone()))).await?;
        }
        if neg.has(Feature::FolderMount) && !p.shares.0.is_empty() {
            write_msg(&mut send, &ctl(Msg::SharedFolders(p.shares.to_pb()))).await?;
        }
        let mut input = conn.open_uni().await?;
        input.set_priority(20)?;
        let mut prelude = Vec::new();
        encode_varint(stream_type::INPUT, &mut prelude);
        input.write_all(&prelude).await?;
        anyhow::Ok(input)
    };
    let mut input = match setup.await {
        Ok(i) => i,
        Err(e) => return End::Lost(format!("{e:#}")),
    };

    let files_on = neg.has(Feature::FileTransfer);
    let images_on = neg.has(Feature::ClipboardImage);
    // Copy on one side, paste on the other (folders too), both directions.
    let clip_on = files_on && neg.has(Feature::ClipboardFiles);
    let file_flags = (files_on, images_on, clip_on, neg.has(Feature::Print));
    let downloads = Arc::new(crate::transfer::Downloads::default());
    let uni = tokio::spawn(accept_uni(
        conn.clone(),
        sinks.video.clone(),
        sinks.ui.clone(),
        sinks.stats.clone(),
        sinks.clip.clone(),
        downloads.clone(),
        file_flags,
        neg.has(Feature::MultiStream),
    ));
    // Where files go: the TCP file channel once the host offered it and it
    // is up (FEATURE_TCP_FILES), FILE streams on this connection until then.
    let files_link = nya_transport::files::FileLink::new(conn.clone());
    let mut file_channel_task: Option<tokio::task::JoinHandle<()>> = None;
    // Control messages from spawned tasks (failed clipboard sends).
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<pb::ControlMsg>();
    let usb_on = neg.has(Feature::UsbRedirect);
    // The host reads our shared folders only if we shared some.
    let shares = (neg.has(Feature::FolderMount) && !p.shares.0.is_empty()).then(|| p.shares.clone());
    let bidi = tokio::spawn({
        let conn = conn.clone();
        async move {
            while let Ok((send, mut recv)) = conn.accept_bi().await {
                let shares = shares.clone();
                tokio::spawn(async move {
                    match read_varint(&mut recv).await {
                        Ok(Some(stream_type::TUNNEL)) if usb_on => {
                            let Ok(Some(port)) = read_varint(&mut recv).await else { return };
                            if let Err(e) = crate::usb::tunnel(send, recv, port).await {
                                tracing::debug!("usb tunnel: {e:#}");
                            }
                        }
                        Ok(Some(stream_type::FS)) if shares.is_some() => {
                            if let Err(e) = nya_transport::folders::serve_stream(send, recv, shares.unwrap()).await {
                                tracing::debug!("folder request: {e:#}");
                            }
                        }
                        _ => {
                            let _ = recv.stop(0u32.into());
                        }
                    }
                });
            }
        }
    });
    let dgram = tokio::spawn(read_datagrams(
        conn.clone(),
        sinks.audio.clone(),
        neg.has(Feature::Audio),
        sinks.video.clone(),
        sinks.stats.clone(),
        neg.has(Feature::VideoDatagram),
    ));
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    let clipboard = neg.has(Feature::ClipboardText);

    let end = loop {
        tokio::select! {
            m = read_msg::<pb::ControlMsg, _>(&mut recv, MAX_MESSAGE_LEN) => {
                let m = match m {
                    Ok(Some(m)) => m,
                    Ok(None) => break End::Lost("被控端关闭了连接".into()),
                    Err(e) => break End::Lost(format!("{e}")),
                };
                match m.msg {
                    Some(Msg::SessionInfo(i)) => sinks.ui.send(UiEvent::SessionInfo(i)),
                    Some(Msg::SessionRole(r)) => sinks.ui.send(UiEvent::Role(r)),
                    // The decoder notices the new stream id itself; frames may arrive first.
                    Some(Msg::StreamStarted(s)) => sinks.ui.send(UiEvent::StreamStarted(s)),
                    Some(Msg::StreamError(e)) => sinks.ui.send(UiEvent::StreamError(e.slot, e.message)),
                    Some(Msg::DisplayChanged(d)) => tracing::info!("host displays changed: {} displays", d.displays.len()),
                    Some(Msg::ServerStats(s)) => sinks.ui.send(UiEvent::ServerStats(s)),
                    Some(Msg::ClipboardText(c)) if clipboard => sinks.ui.send(UiEvent::Clipboard(c.text)),
                    Some(Msg::Pong(p)) => sinks.stats.on_pong(p.t_us, p.server_t_us),
                    Some(Msg::FileOffer(o)) if clip_on => {
                        tracing::info!("host copied {} item(s) (offer {:016x})", o.files.len(), o.transfer_id);
                        sinks.clip.register(&o);
                        sinks.ui.send(UiEvent::ClipOffer(o));
                    }
                    Some(Msg::FileOffer(o)) if files_on => sinks.ui.send(UiEvent::FileOffer(o)),
                    Some(Msg::FileRequest(req)) if clip_on => {
                        let items = sinks.clip.outgoing.lock().unwrap().items(req.transfer_id);
                        match items {
                            Some(items) => {
                                tracing::info!("host is pasting our files (offer {:016x})", req.transfer_id);
                                tokio::spawn(crate::transfer::send_clipboard_files(files_link.clone(), req.transfer_id, items, sinks.ui.clone(), internal_tx.clone()));
                            }
                            None => {
                                let r = pb::FileResult { transfer_id: req.transfer_id, ok: false, message: "这批文件已过期，请在客户端重新复制".into(), saved_to: String::new() };
                                let _ = internal_tx.send(ctl(Msg::FileResult(r)));
                            }
                        }
                    }
                    Some(Msg::FileResult(r)) => {
                        if !r.ok {
                            if let Some(Err(e)) = sinks.clip.incoming.fail(r.transfer_id, r.message.clone()) {
                                sinks.clip.finish(r.transfer_id, Err(e));
                            }
                        }
                        sinks.ui.send(UiEvent::FileResult(r));
                    }
                    Some(Msg::UsbStatus(u)) => sinks.ui.send(UiEvent::UsbStatus(u)),
                    Some(Msg::FolderMountStatus(s)) => sinks.ui.send(UiEvent::FolderMount(s)),
                    Some(Msg::FileChannel(fc)) if neg.has(Feature::TcpFiles) => {
                        // Same address and port as QUIC (a port forward needs both).
                        let addr = conn.remote_address();
                        let (identity, pinned, link) = (p.identity.clone(), p.pinned, files_link.clone());
                        let (ui, downloads, clip) = (sinks.ui.clone(), downloads.clone(), sinks.clip.clone());
                        if let Some(t) = file_channel_task.take() {
                            t.abort();
                        }
                        file_channel_task = Some(tokio::spawn(async move {
                            let on_file: nya_transport::filechan::OnFile = Arc::new(move |h, mut r| {
                                let (ui, downloads, clip) = (ui.clone(), downloads.clone(), clip.clone());
                                tokio::spawn(async move { crate::transfer::receive_body(h, &mut r, ui, downloads, clip, file_flags).await });
                            });
                            match nya_transport::filechan::connect(addr, &identity, pinned, &fc.token, on_file).await {
                                Ok(ch) => {
                                    tracing::info!("files go over the TCP file channel ({addr})");
                                    link.set_tcp(Some(ch));
                                }
                                Err(e) => tracing::warn!("file channel (TCP {addr}): {e:#}; files go over QUIC"),
                            }
                        }));
                    }
                    Some(Msg::GamepadRumble(r)) => sinks.ui.send(UiEvent::GamepadRumble(r)),
                    Some(Msg::Bye(b)) => break End::Fatal(format!("被控端断开：{}", b.reason)),
                    Some(other) => tracing::debug!("ignoring {other:?}"),
                    None => tracing::debug!("ignoring unknown control message"),
                }
            }
            c = cmds.recv() => match c {
                Some(NetCmd::Input(m)) => {
                    if let Err(e) = write_msg(&mut input, &m).await {
                        break End::Lost(format!("input: {e}"));
                    }
                }
                Some(NetCmd::Control(m)) => {
                    track(p, &m);
                    if matches!(m.msg, Some(Msg::ClipboardText(_))) && !clipboard {
                        continue;
                    }
                    if let Err(e) = write_msg(&mut send, &m).await {
                        break End::Lost(format!("control: {e}"));
                    }
                }
                Some(NetCmd::SendFiles(paths)) => {
                    if files_on {
                        tokio::spawn(crate::transfer::upload(files_link.clone(), paths, sinks.ui.clone()));
                    } else {
                        sinks.ui.send(UiEvent::FileResult(pb::FileResult {
                            ok: false,
                            message: "被控端版本不支持文件传输，请升级被控端".into(),
                            ..Default::default()
                        }));
                    }
                }
                Some(NetCmd::Mic(d)) => {
                    if neg.has(Feature::Microphone) {
                        let _ = conn.send_datagram(d.into());
                    }
                }
                Some(NetCmd::SendImage(dib)) => {
                    if images_on {
                        tokio::spawn(crate::transfer::send_image(files_link.clone(), dib));
                    }
                }
                Some(NetCmd::OfferFiles(paths)) => {
                    if clip_on {
                        let offer = sinks.clip.outgoing.lock().unwrap().offer(&paths, true);
                        if let Some(o) = offer {
                            tracing::info!("offering {} copied item(s) to the host", o.files.len());
                            if let Err(e) = write_msg(&mut send, &ctl(Msg::FileOffer(o))).await {
                                break End::Lost(format!("control: {e}"));
                            }
                        }
                    }
                }
                Some(NetCmd::ClipboardPaste(id, reply)) => {
                    use nya_transport::clipfiles::Paste;
                    match sinks.clip.incoming.paste(id) {
                        Paste::Ready(p) => { let _ = reply.send(Ok(p)); }
                        Paste::Request => {
                            sinks.clip.restart(id);
                            sinks.clip.wait(id, reply);
                            let req = pb::FileRequest { transfer_id: id, purpose: pb::FilePurpose::Clipboard as i32 };
                            if let Err(e) = write_msg(&mut send, &ctl(Msg::FileRequest(req))).await {
                                break End::Lost(format!("control: {e}"));
                            }
                        }
                        Paste::Wait => sinks.clip.wait(id, reply),
                        Paste::Failed(e) => { let _ = reply.send(Err(e)); }
                        Paste::Unknown => { let _ = reply.send(Err("这批文件已过期，请在被控端重新复制".into())); }
                    }
                }
                Some(NetCmd::Quit) | None => {
                    let _ = write_msg(&mut send, &ctl(Msg::Bye(pb::Bye { reason: "用户断开".into() }))).await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    conn.close(0u32.into(), b"bye");
                    break End::UserQuit;
                }
            },
            Some(m) = internal_rx.recv() => {
                if let Err(e) = write_msg(&mut send, &m).await {
                    break End::Lost(format!("control: {e}"));
                }
            }
            _ = ping.tick() => {
                let _ = write_msg(&mut send, &ctl(Msg::Ping(pb::Ping { t_us: nya_proto::now_us() }))).await;
            }
            e = conn.closed() => break End::Lost(format!("{e}")),
        }
    };
    uni.abort();
    bidi.abort();
    dgram.abort();
    if let Some(t) = file_channel_task {
        t.abort();
    }
    if let Some(ch) = files_link.tcp() {
        ch.close().await;
    }
    // The host starts over after a reconnect: pastes in progress can't finish.
    sinks.clip.fail_all("与被控端的连接断开了");
    end
}

async fn accept_uni(
    conn: Connection,
    video: Arc<VideoRoutes>,
    ui: Ui,
    stats: Arc<Shared>,
    clip: Arc<crate::transfer::ClipFiles>,
    downloads: Arc<crate::transfer::Downloads>,
    flags: (bool, bool, bool, bool),
    multi: bool,
) {
    while let Ok(mut r) = conn.accept_uni().await {
        let (video, ui, stats, downloads, clip) = (video.clone(), ui.clone(), stats.clone(), downloads.clone(), clip.clone());
        tokio::spawn(async move {
            match read_varint(&mut r).await {
                Ok(Some(stream_type::FILE)) => crate::transfer::receive(r, ui, downloads, clip, flags).await,
                Ok(Some(stream_type::VIDEO)) => {
                    let Ok(Some(stream_id)) = read_varint(&mut r).await else { return };
                    // With FEATURE_MULTI_STREAM the prelude names the window (slot).
                    let slot = if multi {
                        let Ok(Some(slot)) = read_varint(&mut r).await else { return };
                        slot as u32
                    } else {
                        0
                    };
                    let Some(video) = video.get(slot) else {
                        tracing::debug!("ignoring video stream {stream_id}: no window for slot {slot}");
                        let _ = r.stop(0u32.into());
                        return;
                    };
                    tracing::info!("video stream {stream_id} (slot {slot}) opened by host");
                    loop {
                        let mut len = [0u8; 4];
                        if r.read_exact(&mut len).await.is_err() {
                            return;
                        }
                        let len = u32::from_le_bytes(len) as usize;
                        if len < VideoFrameHeader::LEN_V1 || len > MAX_VIDEO_FRAME_LEN {
                            tracing::warn!("bad video frame length {len}");
                            return;
                        }
                        let mut buf = vec![0u8; len];
                        if r.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        deliver(&video, &stats, stream_id, buf);
                    }
                }
                Ok(Some(stream_type::CURSOR)) => {
                    while let Ok(Some(m)) = read_msg::<pb::CursorMsg, _>(&mut r, MAX_MESSAGE_LEN).await {
                        ui.send(UiEvent::Cursor(m));
                    }
                }
                Ok(Some(other)) => {
                    tracing::debug!("unknown stream type {other}");
                    let _ = r.stop(0u32.into());
                }
                _ => {}
            }
        });
    }
}

/// A received frame (video frame header + payload) to its window's decoder.
fn deliver(video: &Sender<VideoIn>, stats: &Shared, stream_id: u64, buf: Vec<u8>) {
    let len = buf.len() as u64;
    let first = stats.with(|s| {
        s.bytes += len;
        s.total_rx_bytes += len;
        s.total_rx_frames += 1;
        s.total_rx_frames == 1
    });
    if first {
        tracing::info!("first video frame received: stream {stream_id}, {len} bytes");
    }
    if video.try_send(VideoIn::Frame { stream_id, buf }).is_err() {
        tracing::warn!("decoder queue full; dropping frame");
    }
}

/// Audio, and video sent as datagrams with FEC (FEATURE_VIDEO_DATAGRAM).
async fn read_datagrams(
    conn: Connection,
    audio: Sender<AudioPacket>,
    audio_on: bool,
    video: Arc<VideoRoutes>,
    stats: Arc<Shared>,
    video_on: bool,
) {
    let mut frames = nya_transport::videodgram::Reassembler::new(MAX_VIDEO_FRAME_LEN);
    let mut published = Instant::now();
    let mut first = true;
    while let Ok(d) = conn.read_datagram().await {
        match d.first() {
            Some(&datagram_type::AUDIO) if audio_on => {
                if let Some(p) = AudioPacket::decode(&d) {
                    let _ = audio.try_send(p);
                }
            }
            Some(&datagram_type::VIDEO) if video_on => {
                if first {
                    first = false;
                    tracing::info!("video arrives as datagrams with FEC");
                }
                if let Some(f) = frames.push(&d) {
                    match video.get(f.slot as u32) {
                        Some(tx) => deliver(&tx, &stats, f.stream_id, f.data),
                        None => tracing::debug!("ignoring video frame for slot {}: no window", f.slot),
                    }
                }
                if published.elapsed() >= Duration::from_millis(250) {
                    published = Instant::now();
                    let st = frames.take_stats();
                    stats.with(|s| s.dgram.add(&st));
                }
            }
            _ => {}
        }
    }
}
