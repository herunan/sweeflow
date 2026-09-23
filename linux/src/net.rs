//! QUIC connection to the Mac host. Runs on its own tokio runtime; talks to the Wayland thread
//! through channels.

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::{
    config::Config,
    decode::{Decoder, GpuFrame},
    proto::{self, t, R, W},
};

#[derive(Debug, Clone)]
pub struct WinInfo {
    pub wid: u32,
    pub bundle_id: String,
    pub app_name: String,
    pub title: String,
    pub w: f32,
    pub h: f32,
    pub parent: u32,
    pub rel_x: f32,
    pub rel_y: f32,
    pub kind: u8,
}

pub enum UiEvent {
    Connected(String),
    Disconnected(String),
    WindowAdded(WinInfo),
    WindowChanged { wid: u32, title: String, w: f32, h: f32, rel_x: f32, rel_y: f32 },
    WindowRemoved(u32),
    AppIcon { bundle_id: String, png: Vec<u8> },
    Cursor { hot_x: f32, hot_y: f32, scale: f32, png: Vec<u8> },
    Focused(u32),
    Frame(u32, GpuFrame),
}

pub enum NetCmd {
    /// A framed control-stream message.
    Control(Vec<u8>),
    Subscribe(u32),
    Unsubscribe(u32),
    /// A framed message for a window's video stream.
    Video(u32, Vec<u8>),
}

pub type UiTx = smithay_client_toolkit::reexports::calloop::channel::Sender<UiEvent>;

pub fn spawn(cfg: Config, ui: UiTx) -> mpsc::UnboundedSender<NetCmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(run(cfg, ui, rx));
        })
        .unwrap();
    tx
}

async fn run(cfg: Config, ui: UiTx, mut cmds: mpsc::UnboundedReceiver<NetCmd>) {
    let endpoint = match make_endpoint(&cfg) {
        Ok(e) => e,
        Err(e) => {
            let _ = ui.send(UiEvent::Disconnected(format!("{e:#}")));
            return;
        }
    };
    let mut backoff = Duration::from_millis(250);
    loop {
        match session(&cfg, &endpoint, &ui, &mut cmds).await {
            Ok(()) => return, // UI went away
            Err(e) => {
                log::warn!("connection lost: {e:#}");
                let _ = ui.send(UiEvent::Disconnected(format!("{e:#}")));
            }
        }
        // Drop commands meant for the dead connection.
        while cmds.try_recv().is_ok() {}
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(3));
    }
}

async fn session(
    cfg: &Config,
    endpoint: &quinn::Endpoint,
    ui: &UiTx,
    cmds: &mut mpsc::UnboundedReceiver<NetCmd>,
) -> Result<()> {
    let addr: SocketAddr = tokio::net::lookup_host(&cfg.host)
        .await?
        .next()
        .context("host did not resolve")?;
    let conn = endpoint.connect(addr, "sweeflow")?.await.context("connect")?;
    log::info!("connected to {addr}, rtt {:?}", conn.rtt());

    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    ctl_tx.write_all(&W::new(t::HELLO).u16(proto::VERSION).f32(cfg.scale as f32).f32(cfg.capture_scale as f32).finish()).await?;

    let ui2 = ui.clone();
    let mut reader = tokio::spawn(async move {
        while let Some((ty, body)) = proto::read_msg(&mut ctl_rx).await? {
            if let Err(e) = handle_control(ty, &body, &ui2) {
                log::warn!("bad control message {ty:#x}: {e}");
            }
        }
        anyhow::Ok(())
    });

    let mut video: HashMap<u32, (quinn::SendStream, tokio::task::JoinHandle<()>)> = HashMap::new();
    let render_node = cfg.render_node.clone();
    let result = loop {
        tokio::select! {
            r = &mut reader => break r.map_err(anyhow::Error::from).and_then(|r| r).and(Err(anyhow::anyhow!("host closed the connection"))),
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                match cmd {
                    NetCmd::Control(m) => ctl_tx.write_all(&m).await?,
                    NetCmd::Subscribe(wid) => {
                        if video.contains_key(&wid) { continue; }
                        let (mut tx, rx) = conn.open_bi().await?;
                        tx.write_all(&W::new(t::SUBSCRIBE).u32(wid).finish()).await?;
                        let task = tokio::spawn(video_task(wid, rx, ui.clone(), render_node.clone()));
                        video.insert(wid, (tx, task));
                    }
                    NetCmd::Unsubscribe(wid) => {
                        if let Some((mut tx, task)) = video.remove(&wid) {
                            let _ = tx.finish();
                            task.abort();
                        }
                    }
                    NetCmd::Video(wid, m) => {
                        if let Some((tx, _)) = video.get_mut(&wid) {
                            if tx.write_all(&m).await.is_err() { video.remove(&wid); }
                        }
                    }
                }
            }
        }
    };
    for (_, (_, task)) in video.drain() {
        task.abort();
    }
    conn.close(0u32.into(), b"bye");
    result
}

fn handle_control(ty: u8, body: &[u8], ui: &UiTx) -> Result<()> {
    let mut r = R::new(body);
    let ev = match ty {
        t::WELCOME => {
            let _v = r.u16()?;
            UiEvent::Connected(r.str()?)
        }
        t::WINDOW_ADDED => {
            let wid = r.u32()?;
            let _pid = r.i32()?;
            UiEvent::WindowAdded(WinInfo {
                wid,
                bundle_id: r.str()?,
                app_name: r.str()?,
                title: r.str()?,
                w: r.f32()?,
                h: r.f32()?,
                parent: r.u32()?,
                rel_x: r.f32()?,
                rel_y: r.f32()?,
                kind: r.u8()?,
            })
        }
        t::WINDOW_CHANGED => UiEvent::WindowChanged {
            wid: r.u32()?,
            title: r.str()?,
            w: r.f32()?,
            h: r.f32()?,
            rel_x: r.f32()?,
            rel_y: r.f32()?,
        },
        t::WINDOW_REMOVED => UiEvent::WindowRemoved(r.u32()?),
        t::APP_ICON => UiEvent::AppIcon { bundle_id: r.str()?, png: r.bytes()?.to_vec() },
        t::CURSOR => UiEvent::Cursor { hot_x: r.f32()?, hot_y: r.f32()?, scale: r.f32()?, png: r.bytes()?.to_vec() },
        t::FOCUSED => UiEvent::Focused(r.u32()?),
        t::PONG => return Ok(()),
        _ => {
            log::debug!("ignoring control message {ty:#x}");
            return Ok(());
        }
    };
    ui.send(ev).map_err(|_| anyhow::anyhow!("ui gone"))
}

/// Receives encoded frames for one window and decodes them on a dedicated thread, so a slow
/// decode never stalls the network reader.
async fn video_task(wid: u32, mut rx: quinn::RecvStream, ui: UiTx, render_node: String) {
    // Unbounded is fine: the host never has more than a couple of frames in flight.
    let (ftx, frx) = std::sync::mpsc::channel::<(u64, bool, Vec<u8>)>();
    let ui2 = ui.clone();
    std::thread::Builder::new()
        .name(format!("decode-{wid}"))
        .spawn(move || {
            let mut dec = match Decoder::new(&render_node) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("window {wid}: {e:#}");
                    return;
                }
            };
            let mut waiting_for_key = true;
            while let Ok((capture_us, key, data)) = frx.recv() {
                if waiting_for_key && !key {
                    continue;
                }
                waiting_for_key = false;
                match dec.decode(&data, capture_us) {
                    Ok(Some(f)) => {
                        if ui2.send(UiEvent::Frame(wid, f)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        log::warn!("window {wid}: decode error {e:#}; waiting for keyframe");
                        waiting_for_key = true;
                    }
                }
            }
        })
        .unwrap();

    loop {
        let msg = match proto::read_msg(&mut rx).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => {
                log::debug!("video stream {wid} ended: {e:#}");
                break;
            }
        };
        let (ty, body) = msg;
        if ty != t::FRAME {
            continue;
        }
        let parsed = (|| -> Result<_> {
            let mut r = R::new(&body);
            let capture_us = r.u64()?;
            let _w = r.u16()?;
            let _h = r.u16()?;
            let key = r.u8()? & 1 != 0;
            Ok((capture_us, key, r.bytes()?.to_vec()))
        })();
        match parsed {
            Ok(f) => {
                if ftx.send(f).is_err() {
                    break;
                }
            }
            Err(e) => log::warn!("bad frame: {e}"),
        }
    }
}

// --- TLS -----------------------------------------------------------------------------------

#[derive(Debug)]
struct PinnedHost {
    fingerprint: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedHost {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = Sha256::digest(end.as_ref());
        if got.as_slice() == self.fingerprint.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "host certificate {} does not match the paired Mac; re-run `sweeflow pair`",
                hex::encode(got)
            )))
        }
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn make_endpoint(cfg: &Config) -> Result<quinn::Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let (cert, key) = cfg.client_identity()?;
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedHost {
            fingerprint: hex::decode(&cfg.host_fingerprint).context("bad host fingerprint in config")?,
            provider,
        }))
        .with_client_auth_cert(vec![cert], PrivateKeyDer::Pkcs8(key.into()))?;
    tls.alpn_protocols = vec![b"sweeflow/1".to_vec()];

    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
    transport.stream_receive_window((16u32 << 20).into());
    transport.receive_window((64u32 << 20).into());
    transport.initial_rtt(Duration::from_millis(10));
    let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    client.transport_config(Arc::new(transport));

    let mut ep = quinn::Endpoint::client("[::]:0".parse()?)?;
    ep.set_default_client_config(client);
    Ok(ep)
}
