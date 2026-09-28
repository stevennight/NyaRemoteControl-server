//! USB passthrough on the host (design: phase 3, optional component).
//!
//! The client shares devices with usbipd-win (a USB/IP server). Here a local
//! listener on 127.0.0.1:3240 stands in for it: every TCP connection is
//! carried to the client on a QUIC bidi stream (type TUNNEL) and connected to
//! usbipd there. usbip-win2 then attaches devices from 127.0.0.1.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use nya_proto::frame::stream_type;
use nya_proto::framing::encode_varint;
use nya_transport::quinn::Connection;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

pub use nya_server_core::components::usbip_exe;

pub const USBIP_PORT: u16 = 3240;

pub struct UsbHost {
    conn: Connection,
    listener: Option<tokio::task::JoinHandle<()>>,
    /// busid -> usbip-win2 port
    attached: HashMap<String, u32>,
}

async fn run_usbip(args: &[&str]) -> Result<String> {
    let exe = usbip_exe().ok_or_else(|| anyhow!("被控端没有安装 usbip-win2"))?;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(&exe).args(args).kill_on_drop(true).output(),
    )
    .await
    .context("usbip 超时")??;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        bail!("usbip {}: {}", args.join(" "), text.trim());
    }
    Ok(text)
}

/// "succesfully attached to port 2" / "port: 2" → 2
fn parse_port(text: &str) -> Option<u32> {
    let lower = text.to_lowercase();
    let i = lower.rfind("port")?;
    lower[i + 4..].trim_start_matches([' ', ':', '#']).split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

impl UsbHost {
    pub fn new(conn: Connection) -> Self {
        Self { conn, listener: None, attached: HashMap::new() }
    }

    /// Start the local tunnel endpoint (once).
    async fn ensure_listener(&mut self) -> Result<()> {
        if self.listener.as_ref().is_some_and(|l| !l.is_finished()) {
            return Ok(());
        }
        let listener = TcpListener::bind(("127.0.0.1", USBIP_PORT))
            .await
            .with_context(|| format!("端口 {USBIP_PORT} 被占用（被控端是否也运行着 usbipd？）"))?;
        let conn = self.conn.clone();
        self.listener = Some(tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let conn = conn.clone();
                tokio::spawn(async move {
                    if let Err(e) = tunnel(conn, tcp).await {
                        tracing::debug!("usb tunnel: {e:#}");
                    }
                });
            }
        }));
        Ok(())
    }

    pub async fn attach(&mut self, busid: &str) -> Result<String> {
        if !busid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
            bail!("无效的 busid");
        }
        self.ensure_listener().await?;
        let out = run_usbip(&["attach", "-r", "127.0.0.1", "-b", busid]).await?;
        let port = parse_port(&out).unwrap_or(0);
        self.attached.insert(busid.to_owned(), port);
        tracing::info!("usb {busid} attached (port {port})");
        Ok(format!("已连接到被控端（端口 {port}）"))
    }

    pub async fn detach(&mut self, busid: &str) -> Result<String> {
        let Some(port) = self.attached.remove(busid) else { bail!("设备没有连接") };
        if port > 0 {
            run_usbip(&["detach", "-p", &port.to_string()]).await?;
        } else {
            run_usbip(&["detach", "--all"]).await?;
        }
        Ok("已断开".into())
    }

    pub async fn detach_all(&mut self) {
        for (busid, port) in std::mem::take(&mut self.attached) {
            let r = if port > 0 { run_usbip(&["detach", "-p", &port.to_string()]).await } else { run_usbip(&["detach", "--all"]).await };
            if let Err(e) = r {
                tracing::warn!("detach {busid}: {e:#}");
            }
        }
        if let Some(l) = self.listener.take() {
            l.abort();
        }
    }
}

/// Carry one TCP connection to the client's usbipd over a QUIC bidi stream.
async fn tunnel(conn: Connection, mut tcp: tokio::net::TcpStream) -> Result<()> {
    let _ = tcp.set_nodelay(true);
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut prelude = Vec::new();
    encode_varint(stream_type::TUNNEL, &mut prelude);
    encode_varint(USBIP_PORT as u64, &mut prelude);
    send.write_all(&prelude).await?;
    let (mut tr, mut tw) = tcp.split();
    let up = async {
        tokio::io::copy(&mut tr, &mut send).await?;
        send.finish()?;
        anyhow::Ok(())
    };
    let down = async {
        tokio::io::copy(&mut recv, &mut tw).await?;
        tw.shutdown().await?;
        anyhow::Ok(())
    };
    tokio::try_join!(up, down)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(parse_port("succesfully attached to port 2"), Some(2));
        assert_eq!(parse_port("port: 13\n"), Some(13));
        assert_eq!(parse_port("nothing"), None);
    }
}
