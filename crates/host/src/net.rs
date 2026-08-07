//! The host end of the UDP session.
//!
//! The host is the client: it keeps sending `Hello` until the VM answers, then
//! streams microphone packets and consumes speaker packets over the same
//! socket. Because both directions share one 5-tuple, the Windows side only
//! needs a single inbound firewall rule and never has to dial back.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use audiobridge_proto as proto;
use audiobridge_proto::{Direction, Incoming, JitterBuffer, Packet, Status};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::config::HostConfig;

const PING_INTERVAL: Duration = Duration::from_secs(2);
const STATS_INTERVAL: Duration = Duration::from_secs(30);

/// Counters worth printing every now and then.
#[derive(Default)]
pub struct Counters {
    pub mic_packets: AtomicU64,
    pub mic_dropped: AtomicU64,
    pub speaker_packets: AtomicU64,
}

pub struct Bridge {
    pub cfg: Arc<HostConfig>,
    pub mic_rx: mpsc::Receiver<Vec<i16>>,
    pub speaker: Option<Arc<Mutex<JitterBuffer>>>,
    pub counters: Arc<Counters>,
}

pub async fn run(bridge: Bridge) -> Result<()> {
    let Bridge {
        cfg,
        mut mic_rx,
        speaker,
        counters,
    } = bridge;

    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("binding a local UDP port")?;
    socket
        .connect(cfg.server.address)
        .await
        .with_context(|| format!("pointing the socket at {}", cfg.server.address))?;
    tracing::info!(server = %cfg.server.address, local = %socket.local_addr()?, "session socket ready");

    let session = new_session_id();
    let hello = Packet::Hello {
        token: cfg.server.token.as_bytes().to_vec(),
        session,
        mic: cfg.mic.format(),
        speaker: cfg.speaker.format(),
    }
    .encode();

    let mut established = false;
    let mut last_rx = Instant::now();
    let mut seq: u32 = 0;
    let mut pending_ping: Option<(u64, Instant)> = None;
    let mut rtt: Option<Duration> = None;

    let mut hello_timer =
        tokio::time::interval(Duration::from_millis(cfg.server.handshake_timeout_ms));
    let mut ping_timer = tokio::time::interval(PING_INTERVAL);
    let mut stats_timer = tokio::time::interval(STATS_INTERVAL);
    // The first tick of each fires immediately, which is what we want for the
    // handshake and not for the stats dump.
    stats_timer.reset();

    let mut rx_buf = vec![0u8; proto::MAX_PACKET];
    let mut tx_buf = Vec::with_capacity(proto::MAX_PACKET);
    let mut samples = Vec::with_capacity(proto::MAX_PACKET / 2);
    let idle_timeout = Duration::from_millis(cfg.server.idle_timeout_ms);
    let mic_channels = cfg.mic.channels;

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                if established {
                    let _ = socket.send(&Packet::Bye { session }.encode()).await;
                }
                tracing::info!("shutting down");
                return Ok(());
            }

            result = socket.recv(&mut rx_buf) => {
                let n = match result {
                    Ok(n) => n,
                    Err(e) => {
                        // ICMP port-unreachable surfaces here on Linux. It just
                        // means the VM agent is not up yet.
                        tracing::debug!("recv failed: {e}");
                        continue;
                    }
                };
                last_rx = Instant::now();

                match proto::decode(&rx_buf[..n]) {
                    Ok(Incoming::Audio { header, payload }) => {
                        if header.session != session || header.direction != Direction::Speaker {
                            continue;
                        }
                        let Some(jitter) = speaker.as_ref() else { continue };
                        proto::decode_samples(payload, &mut samples);
                        if let Ok(mut jb) = jitter.lock() {
                            jb.push(header.seq, &samples);
                        }
                        counters.speaker_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Incoming::Control(Packet::HelloAck { status, session: s, mic, speaker: spk, message })) => {
                        if s != session {
                            continue;
                        }
                        match status {
                            Status::Ok if !established => {
                                established = true;
                                seq = 0;
                                if let Some(jb) = speaker.as_ref() {
                                    if let Ok(mut jb) = jb.lock() {
                                        jb.reset();
                                    }
                                }
                                tracing::info!(%mic, speaker = %spk, "bridge up: {message}");
                            }
                            Status::Ok => {}
                            other => {
                                // Keep retrying: the operator may be fixing the
                                // far side while we sit here.
                                tracing::error!("VM agent rejected the handshake: {other} ({message})");
                                established = false;
                            }
                        }
                    }
                    Ok(Incoming::Control(Packet::Ping { session: s, nonce })) if s == session => {
                        let _ = socket.send(&Packet::Pong { session, nonce }.encode()).await;
                    }
                    Ok(Incoming::Control(Packet::Pong { session: s, nonce })) if s == session => {
                        if let Some((sent_nonce, at)) = pending_ping {
                            if sent_nonce == nonce {
                                rtt = Some(at.elapsed());
                                pending_ping = None;
                            }
                        }
                    }
                    Ok(Incoming::Control(Packet::Bye { session: s })) if s == session => {
                        tracing::info!("VM agent closed the session");
                        established = false;
                        reset(&speaker);
                    }
                    Ok(Incoming::Control(_)) => {}
                    Err(e) => tracing::debug!("dropping a malformed packet: {e}"),
                }
            }

            Some(pcm) = mic_rx.recv() => {
                if !established {
                    counters.mic_dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                proto::encode_audio(&mut tx_buf, session, Direction::Mic, seq, mic_channels, &pcm);
                seq = seq.wrapping_add(1);
                if let Err(e) = socket.send(&tx_buf).await {
                    tracing::debug!("sending a microphone packet failed: {e}");
                    counters.mic_dropped.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.mic_packets.fetch_add(1, Ordering::Relaxed);
                }
            }

            _ = hello_timer.tick(), if !established => {
                if let Err(e) = socket.send(&hello).await {
                    tracing::debug!("sending Hello failed: {e}");
                }
            }

            _ = ping_timer.tick(), if established => {
                if last_rx.elapsed() > idle_timeout {
                    tracing::warn!(
                        after = ?last_rx.elapsed(),
                        "no packets from the VM agent; re-handshaking"
                    );
                    established = false;
                    pending_ping = None;
                    rtt = None;
                    reset(&speaker);
                    continue;
                }
                let nonce = new_session_id() as u64;
                pending_ping = Some((nonce, Instant::now()));
                let _ = socket.send(&Packet::Ping { session, nonce }.encode()).await;
            }

            _ = stats_timer.tick() => {
                log_stats(&counters, &speaker, established, rtt);
            }
        }
    }
}

fn reset(speaker: &Option<Arc<Mutex<JitterBuffer>>>) {
    if let Some(jb) = speaker.as_ref() {
        if let Ok(mut jb) = jb.lock() {
            jb.reset();
        }
    }
}

fn log_stats(
    counters: &Counters,
    speaker: &Option<Arc<Mutex<JitterBuffer>>>,
    established: bool,
    rtt: Option<Duration>,
) {
    let mic_sent = counters.mic_packets.load(Ordering::Relaxed);
    let mic_dropped = counters.mic_dropped.load(Ordering::Relaxed);
    let spk = counters.speaker_packets.load(Ordering::Relaxed);

    let jitter = speaker.as_ref().and_then(|jb| {
        jb.lock()
            .ok()
            .map(|jb| (jb.stats(), jb.fill_frames(), jb.rate()))
    });

    match jitter {
        Some((stats, fill, rate)) => tracing::info!(
            up = established,
            rtt = ?rtt,
            mic_sent,
            mic_dropped,
            spk_recv = spk,
            spk_lost = stats.lost,
            spk_late = stats.late,
            spk_underruns = stats.underruns,
            spk_overruns = stats.overruns,
            spk_resyncs = stats.resyncs,
            spk_fill_frames = fill,
            spk_rate = format!("{rate:.5}"),
            "stats"
        ),
        None => tracing::info!(up = established, rtt = ?rtt, mic_sent, mic_dropped, "stats"),
    }
}

/// A per-process identifier that survives restarts without a random source:
/// the far side only needs it to tell one run of the agent from the next.
fn new_session_id() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
        .unwrap_or(0);
    nanos ^ std::process::id().rotate_left(16)
}
