//! Network side of the host: accept QUIC connections, run the handshake
//! (version negotiation + pairing), then bridge the client with the hub.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use nya_proto::frame::stream_type;
use nya_proto::framing::{encode_varint, expect_msg, read_msg, read_varint, write_msg};
use nya_proto::negotiate::{self, LocalVersion, Negotiated};
use nya_proto::pb::{self, control_msg::Msg, Feature};
use nya_proto::MAX_MESSAGE_LEN;
use nya_transport::identity::peer_fingerprint;
use nya_transport::pairing::{self, Transcript};
use nya_transport::quinn::{self, Connection, RecvStream, SendStream};
use nya_transport::{Fingerprint, Identity};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::auth::AuthStore;
use crate::hub::Hub;
use crate::ipc_pb::{host_command::Cmd, host_event::Ev, FrameSent, SetAudio};

pub struct NetConfig {
    pub bind: SocketAddr,
    pub server_name: String,
}

pub async fn serve(cfg: NetConfig, identity: Identity, auth: Arc<AuthStore>, hub: Arc<Hub>) -> Result<()> {
    let endpoint = nya_transport::endpoint::server_endpoint(cfg.bind, &identity)?;
    serve_endpoint(endpoint, cfg.server_name, identity, auth, hub).await
}

pub async fn serve_endpoint(
    endpoint: quinn::Endpoint,
    server_name: String,
    identity: Identity,
    auth: Arc<AuthStore>,
    hub: Arc<Hub>,
) -> Result<()> {
    tracing::info!(
        "listening on UDP {} (certificate {})",
        endpoint.local_addr()?,
        identity.fingerprint()
    );
    let name = Arc::new(server_name);
    while let Some(incoming) = endpoint.accept().await {
        let (auth, hub, name) = (auth.clone(), hub.clone(), name.clone());
        tokio::spawn(async move {
            let remote = incoming.remote_address();
            match incoming.await {
                Ok(conn) => {
                    if let Err(e) = handle(conn, auth, hub, &name).await {
                        tracing::info!("{remote}: session ended: {e:#}");
                    }
                }
                Err(e) => tracing::debug!("{remote}: handshake failed: {e}"),
            }
        });
    }
    Ok(())
}

fn ctl(msg: Msg) -> pb::ControlMsg {
    pb::ControlMsg { msg: Some(msg) }
}

/// Version negotiation and (if needed) pairing. Returns the negotiated session.
async fn handshake(
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    auth: &AuthStore,
    server_name: &str,
    server_fp: Fingerprint,
) -> Result<(Negotiated, pb::Hello)> {
    let hello: pb::Hello = timeout(Duration::from_secs(10), expect_msg(recv, MAX_MESSAGE_LEN))
        .await
        .context("hello timeout")??;
    let client_fp = peer_fingerprint(conn).ok_or_else(|| anyhow!("no client certificate"))?;
    let negotiated = match negotiate::negotiate(&hello, &LocalVersion::current()) {
        Ok(n) => n,
        Err(reject) => {
            tracing::warn!("rejecting {}: {}", hello.client_name, reject.message);
            write_msg(send, &pb::HelloReply { reply: Some(pb::hello_reply::Reply::Reject(reject)) }).await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            bail!("version mismatch");
        }
    };

    let paired = auth.is_paired(&client_fp);
    if !paired && auth.locked_out() {
        let reject = pb::Reject {
            reason: pb::RejectReason::AuthFailed as i32,
            message: "配对失败次数过多，请 10 分钟后再试".into(),
            server_proto_major: nya_proto::PROTO_MAJOR,
            server_min_proto_major: nya_proto::MIN_PROTO_MAJOR,
        };
        write_msg(send, &pb::HelloReply { reply: Some(pb::hello_reply::Reply::Reject(reject)) }).await?;
        let _ = send.finish();
        bail!("pairing locked out");
    }
    let welcome = pb::Welcome {
        proto_major: negotiated.major,
        proto_minor: negotiated.minor,
        server_name: server_name.to_owned(),
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        features: negotiated.features.iter().copied().collect(),
        needs_pairing: !paired,
    };
    write_msg(send, &pb::HelloReply { reply: Some(pb::hello_reply::Reply::Welcome(welcome)) }).await?;

    if !paired {
        let server_nonce = pairing::nonce();
        write_msg(send, &ctl(Msg::AuthChallenge(pb::AuthChallenge { server_nonce: server_nonce.to_vec() }))).await?;
        // The user may need a while to type the code.
        let resp: pb::ControlMsg = timeout(Duration::from_secs(300), expect_msg(recv, MAX_MESSAGE_LEN))
            .await
            .context("pairing timeout")??;
        let Some(Msg::AuthResponse(r)) = resp.msg else { bail!("expected AuthResponse") };
        let t = Transcript { server_nonce: &server_nonce, client_nonce: &r.client_nonce, server_fp, client_fp };
        if r.client_nonce.len() != pairing::NONCE_LEN || !t.verify_client(auth.key(), &r.mac) {
            auth.record_failure();
            tokio::time::sleep(Duration::from_secs(1)).await;
            write_msg(
                send,
                &ctl(Msg::AuthResult(pb::AuthResult { ok: false, server_mac: vec![], message: "配对码错误".into() })),
            )
            .await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            bail!("wrong pairing code from {}", hello.client_name);
        }
        auth.add(&client_fp, &hello.client_name)?;
        tracing::info!("paired new client {} ({client_fp})", hello.client_name);
        write_msg(
            send,
            &ctl(Msg::AuthResult(pb::AuthResult { ok: true, server_mac: t.server_mac(auth.key()), message: String::new() })),
        )
        .await?;
    }
    Ok((negotiated, hello))
}

async fn handle(conn: Connection, auth: Arc<AuthStore>, hub: Arc<Hub>, server_name: &str) -> Result<()> {
    let remote = conn.remote_address();
    let server_fp = auth_server_fp(&conn)?;
    let (mut send, mut recv) = timeout(Duration::from_secs(10), conn.accept_bi()).await.context("control stream timeout")??;
    let (neg, hello) = handshake(&conn, &mut send, &mut recv, &auth, server_name, server_fp).await?;
    tracing::info!(
        "{remote}: client {} {} (proto {}.{}, features {:?})",
        hello.client_name,
        hello.client_version,
        neg.major,
        neg.minor,
        neg.features
    );

    let att = hub.attach();
    let token = att.token;
    let result = run_session(&conn, send, recv, &hub, att, &neg).await;
    hub.detach(token);
    conn.close(0u32.into(), b"bye");
    result
}

/// Our own certificate fingerprint as seen on this connection (for the pairing transcript).
fn auth_server_fp(conn: &Connection) -> Result<Fingerprint> {
    // quinn doesn't expose the local certificate; the service identity is loaded once.
    let _ = conn;
    SERVER_FP.get().copied().ok_or_else(|| anyhow!("server identity not set"))
}

pub static SERVER_FP: std::sync::OnceLock<Fingerprint> = std::sync::OnceLock::new();

/// Everything the client asked for, so it can be replayed when the helper restarts.
#[derive(Default)]
struct Replay {
    caps: Option<pb::ClientCaps>,
    start: Option<pb::StartStream>,
    audio: bool,
}

async fn run_session(
    conn: &Connection,
    send: SendStream,
    mut recv: RecvStream,
    hub: &Arc<Hub>,
    mut att: crate::hub::Attachment,
    neg: &Negotiated,
) -> Result<()> {
    // Control writer task.
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<pb::ControlMsg>(64);
    let mut ctl_send = send;
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if write_msg(&mut ctl_send, &m).await.is_err() {
                break;
            }
        }
    });

    let info = hub.session_info.borrow().clone();
    if let Some(info) = info {
        let _ = ctl_tx.send(ctl(Msg::SessionInfo(info))).await;
    }

    // Video / cursor writer tasks.
    let (video_tx, video_rx) = mpsc::channel::<crate::ipc_pb::VideoFrame>(4);
    let (cursor_tx, cursor_rx) = mpsc::channel::<pb::CursorMsg>(256);
    let video_task = tokio::spawn(video_writer(conn.clone(), video_rx, hub.clone()));
    let cursor_task = tokio::spawn(cursor_writer(conn.clone(), cursor_rx));
    let input_task = tokio::spawn(input_reader(conn.clone(), hub.clone()));

    let mut replay = Replay::default();
    let mut generation = hub.generation.subscribe();
    generation.mark_unchanged();
    let audio_on = neg.has(Feature::Audio);
    let clipboard_on = neg.has(Feature::ClipboardText);

    let outcome: Result<()> = loop {
        tokio::select! {
            m = read_msg::<pb::ControlMsg, _>(&mut recv, MAX_MESSAGE_LEN) => {
                let m = match m {
                    Ok(Some(m)) => m,
                    Ok(None) => break Ok(()),
                    Err(e) => break Err(e.into()),
                };
                match m.msg {
                    Some(Msg::ClientCaps(c)) => {
                        replay.caps = Some(c.clone());
                        hub.send(Cmd::ClientCaps(c));
                        if audio_on {
                            replay.audio = true;
                            hub.send(Cmd::SetAudio(SetAudio { enabled: true }));
                        }
                    }
                    Some(Msg::StartStream(s)) => {
                        replay.start = Some(s.clone());
                        hub.send(Cmd::StartStream(s));
                    }
                    Some(Msg::StopStream(s)) => {
                        replay.start = None;
                        hub.send(Cmd::StopStream(s));
                    }
                    Some(Msg::SetMode(m)) => {
                        if let Some(s) = replay.start.as_mut() {
                            s.config.get_or_insert_with(Default::default).mode = m.mode;
                        }
                        hub.send(Cmd::SetMode(m));
                    }
                    Some(Msg::RequestKeyframe(k)) => hub.send(Cmd::RequestKeyframe(k)),
                    Some(Msg::Ping(p)) => {
                        let _ = ctl_tx.try_send(ctl(Msg::Pong(pb::Pong { t_us: p.t_us, server_t_us: nya_proto::now_us() })));
                    }
                    Some(Msg::ClientStats(s)) => tracing::debug!("client stats: {s:?}"),
                    Some(Msg::ClipboardText(c)) if clipboard_on => hub.send(Cmd::Clipboard(c)),
                    Some(Msg::SendSas(_)) if neg.has(Feature::Sas) => {
                        if let Err(e) = crate::winutil::send_sas() {
                            tracing::warn!("SendSAS: {e:#}");
                        }
                    }
                    Some(Msg::Bye(b)) => {
                        tracing::info!("client said bye: {}", b.reason);
                        break Ok(());
                    }
                    Some(other) => tracing::debug!("ignoring control message {other:?}"),
                    None => tracing::debug!("ignoring unknown control message"),
                }
            }
            ev = att.events.recv() => {
                let Some(ev) = ev else { break Ok(()) };
                match ev.ev {
                    Some(Ev::Video(f)) => {
                        if video_tx.send(f).await.is_err() {
                            break Err(anyhow!("video writer stopped"));
                        }
                    }
                    Some(Ev::Cursor(c)) => {
                        let _ = cursor_tx.try_send(c);
                    }
                    Some(Ev::Audio(a)) => {
                        if audio_on {
                            if let Err(e) = conn.send_datagram(a.datagram.into()) {
                                tracing::trace!("audio datagram: {e}");
                            }
                        }
                    }
                    Some(Ev::SessionInfo(i)) => { let _ = ctl_tx.send(ctl(Msg::SessionInfo(i))).await; }
                    Some(Ev::StreamStarted(s)) => { let _ = ctl_tx.send(ctl(Msg::StreamStarted(s))).await; }
                    Some(Ev::StreamError(e)) => { let _ = ctl_tx.send(ctl(Msg::StreamError(e))).await; }
                    Some(Ev::DisplayChanged(d)) => { let _ = ctl_tx.send(ctl(Msg::DisplayChanged(d))).await; }
                    Some(Ev::Stats(s)) => { let _ = ctl_tx.try_send(ctl(Msg::ServerStats(s))); }
                    Some(Ev::Clipboard(c)) => {
                        if clipboard_on {
                            let _ = ctl_tx.send(ctl(Msg::ClipboardText(c))).await;
                        }
                    }
                    None => {}
                }
            }
            _ = generation.changed() => {
                // The helper restarted (session switch): replay the client's requests.
                tracing::info!("host restarted; replaying stream request");
                if let Some(c) = &replay.caps { hub.send(Cmd::ClientCaps(c.clone())); }
                if replay.audio { hub.send(Cmd::SetAudio(SetAudio { enabled: true })); }
                if let Some(s) = &replay.start { hub.send(Cmd::StartStream(s.clone())); }
            }
            _ = att.kicked.notified() => {
                let _ = ctl_tx.send(ctl(Msg::Bye(pb::Bye { reason: "另一个客户端已连接".into() }))).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                break Ok(());
            }
            e = conn.closed() => break Err(anyhow!("connection closed: {e}")),
        }
    };

    drop(ctl_tx);
    video_task.abort();
    cursor_task.abort();
    input_task.abort();
    let _ = timeout(Duration::from_millis(200), writer).await;
    outcome
}

/// Writes frames on one uni stream per video stream id; acknowledges each
/// frame to the host once quinn accepted it (flow control, §6.2).
async fn video_writer(conn: Connection, mut rx: mpsc::Receiver<crate::ipc_pb::VideoFrame>, hub: Arc<Hub>) -> Result<()> {
    let mut current: Option<(u64, SendStream)> = None;
    while let Some(f) = rx.recv().await {
        if current.as_ref().map(|c| c.0) != Some(f.stream_id) {
            if let Some((_, mut old)) = current.take() {
                let _ = old.finish();
            }
            let mut s = conn.open_uni().await?;
            s.set_priority(1)?;
            let mut prelude = Vec::new();
            encode_varint(stream_type::VIDEO, &mut prelude);
            encode_varint(f.stream_id, &mut prelude);
            s.write_all(&prelude).await?;
            current = Some((f.stream_id, s));
        }
        let s = &mut current.as_mut().unwrap().1;
        let len = (f.header.len() + f.data.len()) as u32;
        s.write_all(&len.to_le_bytes()).await?;
        s.write_all(&f.header).await?;
        s.write_all(&f.data).await?;
        hub.send(Cmd::FrameSent(FrameSent { frame_id: f.frame_id }));
    }
    Ok(())
}

async fn cursor_writer(conn: Connection, mut rx: mpsc::Receiver<pb::CursorMsg>) -> Result<()> {
    let mut s = conn.open_uni().await?;
    // Cursor updates are tiny and latency sensitive.
    s.set_priority(10)?;
    let mut prelude = Vec::new();
    encode_varint(stream_type::CURSOR, &mut prelude);
    s.write_all(&prelude).await?;
    while let Some(m) = rx.recv().await {
        write_msg(&mut s, &m).await?;
    }
    Ok(())
}

/// Accept client uni streams; the input stream feeds the host.
async fn input_reader(conn: Connection, hub: Arc<Hub>) -> Result<()> {
    loop {
        let mut r = conn.accept_uni().await?;
        let hub = hub.clone();
        tokio::spawn(async move {
            match read_varint(&mut r).await {
                Ok(Some(stream_type::INPUT)) => loop {
                    match read_msg::<pb::InputMsg, _>(&mut r, MAX_MESSAGE_LEN).await {
                        Ok(Some(m)) => hub.send(Cmd::Input(m)),
                        Ok(None) => break,
                        Err(e) => {
                            tracing::debug!("input stream: {e}");
                            break;
                        }
                    }
                },
                Ok(Some(other)) => {
                    tracing::debug!("unknown client stream type {other}");
                    let _ = r.stop(0u32.into());
                }
                _ => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    //! Loopback integration test of the whole network protocol with a fake
    //! host (no GPU involved).

    use super::*;
    use crate::ipc_pb::{HostCommand, HostEvent, VideoFrame};
    use nya_proto::frame::VideoFrameHeader;
    use nya_proto::pb::input_msg::Ev as InEv;
    use nya_transport::pairing::PairingKey;

    async fn fake_host(hub: Arc<Hub>, mut cmds: mpsc::UnboundedReceiver<HostCommand>, inputs: mpsc::UnboundedSender<pb::InputMsg>) {
        let mut sent_acks = 0;
        while let Some(HostCommand { cmd: Some(c) }) = cmds.recv().await {
            match c {
                Cmd::StartStream(s) => {
                    let started = pb::StreamStarted { display_id: s.display_id, stream_id: 7, encoder_name: "fake".into(), ..Default::default() };
                    hub.publish(HostEvent { ev: Some(Ev::StreamStarted(started)) }).await;
                    for i in 1..=3u64 {
                        let h = VideoFrameHeader { frame_id: i, width: 64, height: 64, codec: pb::Codec::H264 as u8, ..Default::default() };
                        let mut hb = Vec::new();
                        h.write(&mut hb);
                        let f = VideoFrame { stream_id: 7, frame_id: i, header: hb, data: vec![i as u8; 1000] };
                        hub.publish(HostEvent { ev: Some(Ev::Video(f)) }).await;
                    }
                }
                Cmd::FrameSent(_) => sent_acks += 1,
                Cmd::Input(m) => {
                    let _ = inputs.send(m);
                }
                Cmd::ClientGone(_) => assert!(sent_acks >= 3 || sent_acks == 0),
                _ => {}
            }
        }
    }

    struct Client {
        _ep: quinn::Endpoint,
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
    }

    async fn connect(addr: SocketAddr, id: &Identity, pin: Fingerprint) -> (Client, pb::Welcome) {
        let ep = nya_transport::endpoint::client_endpoint(addr).unwrap();
        let conn = nya_transport::endpoint::connect(&ep, addr, id, Some(pin)).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = negotiate::hello(&LocalVersion::current(), "test", "0");
        write_msg(&mut send, &hello).await.unwrap();
        let reply: pb::HelloReply = expect_msg(&mut recv, MAX_MESSAGE_LEN).await.unwrap();
        let Some(pb::hello_reply::Reply::Welcome(w)) = reply.reply else { panic!("rejected") };
        (Client { _ep: ep, conn, send, recv }, w)
    }

    async fn pair(c: &mut Client, id: &Identity, server_fp: Fingerprint, key: &PairingKey) -> pb::AuthResult {
        let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
        let Some(Msg::AuthChallenge(ch)) = m.msg else { panic!("no challenge") };
        let cn = pairing::nonce();
        let t = Transcript { server_nonce: &ch.server_nonce, client_nonce: &cn, server_fp, client_fp: id.fingerprint() };
        write_msg(&mut c.send, &ctl(Msg::AuthResponse(pb::AuthResponse { client_nonce: cn.to_vec(), mac: t.client_mac(key) })))
            .await
            .unwrap();
        let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
        let Some(Msg::AuthResult(r)) = m.msg else { panic!("no result") };
        if r.ok {
            assert!(t.verify_server(key, &r.server_mac));
        }
        r
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handshake_pairing_stream_and_input() {
        let dir = std::env::temp_dir().join(format!("nya-net-test-{}", nya_proto::now_us()));
        let server_id = Identity::load_or_create(&dir).unwrap();
        let server_fp = server_id.fingerprint();
        let _ = SERVER_FP.set(server_fp);
        let auth = Arc::new(AuthStore::open(&dir).unwrap());
        let key = auth.key().clone();
        let (hub, cmd_rx) = Hub::new();
        let (in_tx, mut in_rx) = mpsc::unbounded_channel();
        tokio::spawn(fake_host(hub.clone(), cmd_rx, in_tx));
        let ep = nya_transport::endpoint::server_endpoint("127.0.0.1:0".parse().unwrap(), &server_id).unwrap();
        let addr = ep.local_addr().unwrap();
        tokio::spawn(serve_endpoint(ep, "test-host".into(), server_id, auth, hub));

        let client_id = Identity::generate().unwrap();

        // 1. Wrong pairing code is refused.
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.needs_pairing);
        let r = pair(&mut c, &client_id, server_fp, &PairingKey::generate()).await;
        assert!(!r.ok);

        // 2. Correct code pairs.
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(w.needs_pairing);
        assert_eq!(w.server_name, "test-host");
        assert!(pair(&mut c, &client_id, server_fp, &key).await.ok);
        c.conn.close(0u32.into(), b"");

        // 3. Paired client skips pairing; stream + input work.
        let (mut c, w) = connect(addr, &client_id, server_fp).await;
        assert!(!w.needs_pairing);
        write_msg(&mut c.send, &ctl(Msg::ClientCaps(pb::ClientCaps::default()))).await.unwrap();
        write_msg(&mut c.send, &ctl(Msg::StartStream(pb::StartStream { display_id: 1, ..Default::default() }))).await.unwrap();
        loop {
            let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
            if let Some(Msg::StreamStarted(s)) = m.msg {
                assert_eq!(s.stream_id, 7);
                break;
            }
        }
        // Streams arrive in any order (the cursor stream opens at session start).
        let mut r = loop {
            let mut r = c.conn.accept_uni().await.unwrap();
            match read_varint(&mut r).await.unwrap() {
                Some(stream_type::VIDEO) => break r,
                Some(stream_type::CURSOR) => continue,
                other => panic!("unexpected stream type {other:?}"),
            }
        };
        assert_eq!(read_varint(&mut r).await.unwrap(), Some(7));
        for i in 1..=3u64 {
            let mut len = [0u8; 4];
            r.read_exact(&mut len).await.unwrap();
            let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
            r.read_exact(&mut buf).await.unwrap();
            let (h, payload) = VideoFrameHeader::parse(&buf).unwrap();
            assert_eq!(h.frame_id, i);
            assert_eq!(payload.len(), 1000);
        }

        let mut input = c.conn.open_uni().await.unwrap();
        let mut prelude = Vec::new();
        encode_varint(stream_type::INPUT, &mut prelude);
        input.write_all(&prelude).await.unwrap();
        let key_msg = pb::InputMsg { ev: Some(InEv::Key(pb::Key { scancode: 0x1e, extended: false, down: true })) };
        write_msg(&mut input, &key_msg).await.unwrap();
        let got = timeout(Duration::from_secs(5), in_rx.recv()).await.unwrap().unwrap();
        assert_eq!(got, key_msg);

        // Ping/pong.
        write_msg(&mut c.send, &ctl(Msg::Ping(pb::Ping { t_us: 42 }))).await.unwrap();
        loop {
            let m: pb::ControlMsg = expect_msg(&mut c.recv, MAX_MESSAGE_LEN).await.unwrap();
            if let Some(Msg::Pong(p)) = m.msg {
                assert_eq!(p.t_us, 42);
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
