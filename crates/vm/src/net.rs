//! The VM end of the UDP session.
//!
//! One session at a time. The host dials in, the audio devices open, and both
//! directions run until the host says goodbye or stops talking. Everything the
//! far side needs to know comes back in the `HelloAck`, so a misconfigured
//! device name shows up in the *host's* log rather than only in this one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use audiobridge_proto as proto;
use audiobridge_proto::{Direction, Format, Incoming, JitterBuffer, Packet, Status};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::audio::{self, AudioThread};
use crate::config::VmConfig;

const STATS_INTERVAL: Duration = Duration::from_secs(30);
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Playback packets buffered between the capture thread and the socket.
const CAPTURE_QUEUE: usize = 8;

#[derive(Default)]
struct Counters {
    mic_packets: AtomicU64,
    speaker_packets: AtomicU64,
    speaker_dropped: AtomicU64,
}

/// Everything that exists only while a host is connected.
struct Session {
    id: u32,
    peer: SocketAddr,
    mic_format: Format,
    speaker_format: Format,
    jitter: Option<Arc<Mutex<JitterBuffer>>>,
    render: Option<AudioThread>,
    capture: Option<AudioThread>,
    last_rx: Instant,
    seq: u32,
    summary: String,
}

impl Session {
    /// Joins the audio threads, which releases the endpoints for the next run.
    fn shut_down(self) {
        tokio::task::block_in_place(|| {
            if let Some(t) = self.render {
                t.stop();
            }
            if let Some(t) = self.capture {
                t.stop();
            }
        });
    }
}

pub async fn run(cfg: Arc<VmConfig>) -> Result<()> {
    let socket = UdpSocket::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    tracing::info!(listen = %cfg.listen, "waiting for a host");

    let counters = Arc::new(Counters::default());
    let (capture_tx, mut capture_rx) = mpsc::channel::<Vec<i16>>(CAPTURE_QUEUE);

    let mut session: Option<Session> = None;
    let mut rx_buf = vec![0u8; proto::MAX_PACKET];
    let mut tx_buf = Vec::with_capacity(proto::MAX_PACKET);
    let mut samples = Vec::with_capacity(proto::MAX_PACKET / 2);

    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    let mut stats = tokio::time::interval(STATS_INTERVAL);
    let idle_timeout = Duration::from_millis(cfg.idle_timeout_ms);

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                if let Some(s) = session.take() {
                    let _ = socket.send_to(&Packet::Bye { session: s.id }.encode(), s.peer).await;
                    s.shut_down();
                }
                tracing::info!("shutting down");
                return Ok(());
            }

            result = socket.recv_from(&mut rx_buf) => {
                let (n, peer) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!("recv failed: {e}");
                        continue;
                    }
                };

                match proto::decode(&rx_buf[..n]) {
                    Ok(Incoming::Control(Packet::Hello { token, session: id, mic, speaker: spk })) => {
                        handle_hello(
                            &cfg, &socket, &mut session, &capture_tx,
                            peer, token, id, mic, spk,
                        ).await;
                    }
                    Ok(Incoming::Audio { header, payload }) => {
                        let Some(s) = session.as_mut() else { continue };
                        if header.session != s.id || peer != s.peer || header.direction != Direction::Mic {
                            continue;
                        }
                        s.last_rx = Instant::now();
                        if let Some(jitter) = s.jitter.as_ref() {
                            proto::decode_samples(payload, &mut samples);
                            if let Ok(mut jb) = jitter.lock() {
                                jb.push(header.seq, &samples);
                            }
                        }
                        counters.mic_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Incoming::Control(Packet::Ping { session: id, nonce })) => {
                        let Some(s) = session.as_mut() else { continue };
                        if id != s.id || peer != s.peer {
                            continue;
                        }
                        s.last_rx = Instant::now();
                        let _ = socket.send_to(&Packet::Pong { session: id, nonce }.encode(), peer).await;
                    }
                    Ok(Incoming::Control(Packet::Pong { session: id, .. })) => {
                        if let Some(s) = session.as_mut() {
                            if id == s.id && peer == s.peer {
                                s.last_rx = Instant::now();
                            }
                        }
                    }
                    Ok(Incoming::Control(Packet::Bye { session: id })) => {
                        if let Some(s) = session.as_ref() {
                            if id == s.id && peer == s.peer {
                                tracing::info!(%peer, "host closed the session");
                                session.take().unwrap().shut_down();
                            }
                        }
                    }
                    Ok(Incoming::Control(Packet::HelloAck { .. })) => {}
                    Err(e) => tracing::debug!(%peer, "dropping a malformed packet: {e}"),
                }
            }

            Some(pcm) = capture_rx.recv() => {
                let Some(s) = session.as_mut() else { continue };
                proto::encode_audio(
                    &mut tx_buf, s.id, Direction::Speaker, s.seq,
                    s.speaker_format.channels, &pcm,
                );
                s.seq = s.seq.wrapping_add(1);
                if socket.send_to(&tx_buf, s.peer).await.is_err() {
                    counters.speaker_dropped.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.speaker_packets.fetch_add(1, Ordering::Relaxed);
                }
            }

            _ = sweep.tick() => {
                if let Some(s) = session.as_ref() {
                    if s.last_rx.elapsed() > idle_timeout {
                        tracing::warn!(peer = %s.peer, after = ?s.last_rx.elapsed(), "host went quiet; closing the session");
                        session.take().unwrap().shut_down();
                    }
                }
            }

            _ = stats.tick() => {
                log_stats(&counters, session.as_ref());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_hello(
    cfg: &Arc<VmConfig>,
    socket: &UdpSocket,
    session: &mut Option<Session>,
    capture_tx: &mpsc::Sender<Vec<i16>>,
    peer: SocketAddr,
    token: Vec<u8>,
    id: u32,
    mic: Format,
    speaker: Format,
) {
    let reply = |status: Status, message: String| {
        Packet::HelloAck {
            status,
            session: id,
            mic,
            speaker,
            message,
        }
        .encode()
    };

    if !proto::token_matches(&cfg.token, &token) {
        tracing::warn!(%peer, "rejected a handshake with a bad token");
        let _ = socket
            .send_to(&reply(Status::AuthFailed, String::new()), peer)
            .await;
        return;
    }

    for (label, format) in [("mic", mic), ("speaker", speaker)] {
        if let Err(e) = format.validate() {
            let _ = socket
                .send_to(
                    &reply(Status::UnsupportedFormat, format!("{label}: {e}")),
                    peer,
                )
                .await;
            return;
        }
    }

    // A live session belongs to its host until it times out.
    if let Some(s) = session.as_ref() {
        if s.peer != peer && s.last_rx.elapsed() < Duration::from_millis(cfg.idle_timeout_ms) {
            tracing::warn!(%peer, owner = %s.peer, "refused a second host");
            let _ = socket
                .send_to(&reply(Status::Busy, s.peer.to_string()), peer)
                .await;
            return;
        }
    }

    // Repeat Hellos are normal — the host resends until an ack gets through —
    // so only rebuild the audio pipeline when something actually changed.
    let unchanged = session.as_ref().is_some_and(|s| {
        s.id == id && s.peer == peer && s.mic_format == mic && s.speaker_format == speaker
    });

    if !unchanged {
        if let Some(old) = session.take() {
            tracing::info!("replacing session {:08x} with {:08x}", old.id, id);
            old.shut_down();
        }
        match start_session(cfg, capture_tx, peer, id, mic, speaker) {
            Ok(s) => {
                tracing::info!(%peer, %mic, %speaker, "session {id:08x} up: {}", s.summary);
                *session = Some(s);
            }
            Err(e) => {
                let message = format!("{e:#}");
                tracing::error!(%peer, "cannot start the session: {message}");
                let _ = socket
                    .send_to(&reply(Status::ServerError, message), peer)
                    .await;
                return;
            }
        }
    }

    if let Some(s) = session.as_mut() {
        s.last_rx = Instant::now();
        let summary = s.summary.clone();
        let _ = socket.send_to(&reply(Status::Ok, summary), peer).await;
    }
}

fn start_session(
    cfg: &Arc<VmConfig>,
    capture_tx: &mpsc::Sender<Vec<i16>>,
    peer: SocketAddr,
    id: u32,
    mic: Format,
    speaker: Format,
) -> Result<Session> {
    let mut summary = Vec::new();

    let (jitter, render) = if cfg.mic.enabled {
        let jb = Arc::new(Mutex::new(JitterBuffer::new(
            mic.channels as usize,
            mic.rate,
            mic.frame_ms,
            cfg.mic.buffer_ms,
        )));
        let thread = audio::spawn_render(cfg.mic.device().map(str::to_owned), mic, jb.clone())
            .context("opening the virtual microphone feed")?;
        summary.push(format!(
            "mic -> {}",
            cfg.mic.device().unwrap_or("(default render)")
        ));
        (Some(jb), Some(thread))
    } else {
        summary.push("mic disabled".to_string());
        (None, None)
    };

    let capture = if cfg.speaker.enabled {
        let thread = audio::spawn_capture(
            cfg.speaker.device().map(str::to_owned),
            cfg.speaker.mode,
            speaker,
            capture_tx.clone(),
        )
        .context("opening playback capture")?;
        summary.push(format!(
            "speaker <- {:?}:{}",
            cfg.speaker.mode,
            cfg.speaker.device().unwrap_or("(default)")
        ));
        Some(thread)
    } else {
        summary.push("speaker disabled".to_string());
        None
    };

    Ok(Session {
        id,
        peer,
        mic_format: mic,
        speaker_format: speaker,
        jitter,
        render,
        capture,
        last_rx: Instant::now(),
        seq: 0,
        summary: summary.join(", "),
    })
}

fn log_stats(counters: &Counters, session: Option<&Session>) {
    let mic = counters.mic_packets.load(Ordering::Relaxed);
    let spk = counters.speaker_packets.load(Ordering::Relaxed);
    let spk_dropped = counters.speaker_dropped.load(Ordering::Relaxed);

    let Some(s) = session else {
        tracing::info!(mic_recv = mic, spk_sent = spk, "stats (idle)");
        return;
    };

    match s.jitter.as_ref().and_then(|jb| {
        jb.lock()
            .ok()
            .map(|jb| (jb.stats(), jb.fill_frames(), jb.rate()))
    }) {
        Some((stats, fill, rate)) => tracing::info!(
            peer = %s.peer,
            mic_recv = mic,
            mic_lost = stats.lost,
            mic_late = stats.late,
            mic_underruns = stats.underruns,
            mic_overruns = stats.overruns,
            mic_resyncs = stats.resyncs,
            mic_fill_frames = fill,
            mic_rate = format!("{rate:.5}"),
            spk_sent = spk,
            spk_dropped,
            "stats"
        ),
        None => {
            tracing::info!(peer = %s.peer, mic_recv = mic, spk_sent = spk, spk_dropped, "stats")
        }
    }
}
