//! WASAPI backend.
//!
//! Two shapes of stream, both shared-mode with `AUTOCONVERTPCM` so the endpoint
//! is free to run at whatever mix format Windows picked while we keep speaking
//! plain 16-bit PCM at the rate the host negotiated:
//!
//! * **Render** — writes audio from Linux into the render half of a virtual
//!   cable. Event driven: the endpoint's own clock paces us, which is what the
//!   jitter buffer's drift control wants to see.
//! * **Capture** — reads back what Windows is playing, either from a real
//!   capture endpoint or, with `AUDCLNT_STREAMFLAGS_LOOPBACK`, from a render
//!   endpoint. This one polls. Loopback deliberately stops signalling its event
//!   when nothing is playing, so an event-driven loop would stall for as long
//!   as the machine is quiet; polling also lets us keep the far end's buffer
//!   warm by substituting silence (see `emit_due_packets`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use audiobridge_proto::{Format, JitterBuffer};
use wasapi::{
    calculate_period_100ns, initialize_mta, Device, DeviceEnumerator, Direction, SampleType,
    StreamMode, WaveFormat,
};

use crate::audio::{AudioThread, DeviceInfo};
use crate::config::CaptureMode;

/// Wait before retrying after a device fails to open or drops out.
const REOPEN_DELAY: Duration = Duration::from_millis(1000);

/// How long the render loop waits on the endpoint event before looking at the
/// stop flag again. Also bounds how long `AudioThread::stop` blocks.
const EVENT_TIMEOUT_MS: u32 = 200;

/// Consecutive event timeouts tolerated before the endpoint is declared dead
/// and reopened. 25 * 200 ms = 5 s.
const MAX_EVENT_TIMEOUTS: u32 = 25;

/// Poll interval for the capture loop. A third of the smallest packet we send,
/// so a packet is never more than a fraction of its own duration late.
const CAPTURE_POLL: Duration = Duration::from_millis(2);

/// If the capture loop falls this far behind its schedule — a suspend, a long
/// GC pause in some other process, a live migration — restart the schedule
/// instead of emitting a burst of thousands of packets.
const SCHEDULE_RESET: Duration = Duration::from_millis(500);

pub fn spawn_render(
    device: Option<String>,
    format: Format,
    jitter: Arc<Mutex<JitterBuffer>>,
) -> Result<AudioThread> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    let handle = thread::Builder::new()
        .name("wasapi-render".into())
        .spawn(move || {
            if let Err(e) = init_com() {
                tracing::error!("render thread cannot use COM: {e:#}");
                return;
            }
            let mut announced = false;
            while !thread_stop.load(Ordering::Relaxed) {
                match render_loop(&thread_stop, device.as_deref(), &format, &jitter) {
                    Ok(()) => {}
                    Err(e) => {
                        if !announced {
                            tracing::warn!("virtual microphone unavailable: {e:#}");
                            announced = true;
                        }
                        if let Ok(mut jb) = jitter.lock() {
                            jb.reset();
                        }
                        thread::sleep(REOPEN_DELAY);
                        continue;
                    }
                }
                announced = false;
            }
            wasapi::deinitialize();
        })
        .context("spawning the render thread")?;

    Ok(AudioThread::new(stop, handle))
}

pub fn spawn_capture(
    device: Option<String>,
    mode: CaptureMode,
    format: Format,
    tx: tokio::sync::mpsc::Sender<Vec<i16>>,
) -> Result<AudioThread> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    let handle = thread::Builder::new()
        .name("wasapi-capture".into())
        .spawn(move || {
            if let Err(e) = init_com() {
                tracing::error!("capture thread cannot use COM: {e:#}");
                return;
            }
            let mut announced = false;
            while !thread_stop.load(Ordering::Relaxed) {
                match capture_loop(&thread_stop, device.as_deref(), mode, &format, &tx) {
                    Ok(()) => {}
                    Err(e) => {
                        if !announced {
                            tracing::warn!("playback capture unavailable: {e:#}");
                            announced = true;
                        }
                        thread::sleep(REOPEN_DELAY);
                        continue;
                    }
                }
                announced = false;
            }
            wasapi::deinitialize();
        })
        .context("spawning the capture thread")?;

    Ok(AudioThread::new(stop, handle))
}

fn init_com() -> Result<()> {
    initialize_mta()
        .ok()
        .map_err(|e| anyhow!("CoInitializeEx failed: {e}"))
}

fn wave_format(format: &Format) -> WaveFormat {
    WaveFormat::new(
        16,
        16,
        &SampleType::Int,
        format.rate as usize,
        format.channels as usize,
        None,
    )
}

/// The endpoint kind a capture mode reads from. Loopback taps a *render*
/// endpoint; the crate turns a Render device plus a Capture stream into
/// `AUDCLNT_STREAMFLAGS_LOOPBACK` on its own.
fn endpoint_direction(mode: CaptureMode) -> Direction {
    match mode {
        CaptureMode::Loopback => Direction::Render,
        CaptureMode::Capture => Direction::Capture,
    }
}

/// Resolve a configured device name. Exact friendly-name match wins; failing
/// that a unique case-insensitive substring match, so an operator can write
/// `CABLE Input` instead of `CABLE Input (VB-Audio Virtual Cable)`. Ambiguous
/// or missing names produce an error that lists what is actually there.
fn find_device(direction: Direction, name: Option<&str>) -> Result<Device> {
    let enumerator = DeviceEnumerator::new().map_err(|e| anyhow!("{e}"))?;

    let Some(wanted) = name else {
        return enumerator
            .get_default_device(&direction)
            .map_err(|e| anyhow!("no default {direction} device: {e}"));
    };

    let collection = enumerator
        .get_device_collection(&direction)
        .map_err(|e| anyhow!("listing {direction} devices: {e}"))?;
    let count = collection.get_nbr_devices().map_err(|e| anyhow!("{e}"))?;

    let mut names = Vec::with_capacity(count as usize);
    let mut fuzzy = Vec::new();
    for i in 0..count {
        let device = collection
            .get_device_at_index(i)
            .map_err(|e| anyhow!("{e}"))?;
        let friendly = device.get_friendlyname().map_err(|e| anyhow!("{e}"))?;
        if friendly == wanted {
            return Ok(device);
        }
        if friendly.to_lowercase().contains(&wanted.to_lowercase()) {
            fuzzy.push((friendly.clone(), i));
        }
        names.push(friendly);
    }

    match fuzzy.len() {
        1 => {
            tracing::info!(matched = %fuzzy[0].0, "resolved {direction} device \"{wanted}\"");
            collection
                .get_device_at_index(fuzzy[0].1)
                .map_err(|e| anyhow!("{e}"))
        }
        0 => bail!(
            "no {direction} device matches \"{wanted}\". Available: {}",
            names.join(", ")
        ),
        _ => bail!(
            "\"{wanted}\" matches several {direction} devices ({}); use the full name",
            fuzzy
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn render_loop(
    stop: &AtomicBool,
    name: Option<&str>,
    format: &Format,
    jitter: &Arc<Mutex<JitterBuffer>>,
) -> Result<()> {
    let device = find_device(Direction::Render, name)?;
    let friendly = device
        .get_friendlyname()
        .unwrap_or_else(|_| "?".to_string());
    let mut client = device.get_iaudioclient().map_err(|e| anyhow!("{e}"))?;

    let wavefmt = wave_format(format);
    let (default_period, _min_period) = client.get_device_period().map_err(|e| anyhow!("{e}"))?;
    // Three packets of slack in the endpoint buffer, but never below what the
    // engine says it needs.
    let wanted =
        calculate_period_100ns((format.frames_per_packet() * 3) as i64, format.rate as i64);
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: wanted.max(default_period),
    };

    client
        .initialize_client(&wavefmt, &Direction::Render, &mode)
        .map_err(|e| anyhow!("initialising \"{friendly}\" for {format}: {e}"))?;
    let event = client.set_get_eventhandle().map_err(|e| anyhow!("{e}"))?;
    let render = client.get_audiorenderclient().map_err(|e| anyhow!("{e}"))?;
    client.start_stream().map_err(|e| anyhow!("{e}"))?;
    tracing::info!(device = %friendly, %format, "feeding the virtual microphone");

    let channels = format.channels as usize;
    let mut pcm: Vec<i16> = Vec::new();
    let mut timeouts = 0u32;

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            let space = client
                .get_available_space_in_frames()
                .map_err(|e| anyhow!("{e}"))? as usize;
            if space > 0 {
                pcm.clear();
                pcm.resize(space * channels, 0);
                match jitter.lock() {
                    Ok(mut jb) => jb.pull(&mut pcm),
                    Err(_) => bail!("the jitter buffer lock is poisoned"),
                }
                render
                    .write_to_device(space, bytemuck::cast_slice::<i16, u8>(&pcm), None)
                    .map_err(|e| anyhow!("{e}"))?;
            }
            match event.wait_for_event(EVENT_TIMEOUT_MS) {
                Ok(()) => timeouts = 0,
                Err(_) => {
                    timeouts += 1;
                    if timeouts >= MAX_EVENT_TIMEOUTS {
                        bail!("\"{friendly}\" stopped signalling; reopening");
                    }
                }
            }
        }
        Ok(())
    })();

    let _ = client.stop_stream();
    result
}

fn capture_loop(
    stop: &AtomicBool,
    name: Option<&str>,
    mode: CaptureMode,
    format: &Format,
    tx: &tokio::sync::mpsc::Sender<Vec<i16>>,
) -> Result<()> {
    let direction = endpoint_direction(mode);
    let device = find_device(direction, name)?;
    let friendly = device
        .get_friendlyname()
        .unwrap_or_else(|_| "?".to_string());
    let mut client = device.get_iaudioclient().map_err(|e| anyhow!("{e}"))?;

    let wavefmt = wave_format(format);
    let (default_period, _min_period) = client.get_device_period().map_err(|e| anyhow!("{e}"))?;
    let wanted =
        calculate_period_100ns((format.frames_per_packet() * 4) as i64, format.rate as i64);
    let stream_mode = StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: wanted.max(default_period),
    };

    // Always a Capture stream. Against a Render endpoint that is what makes it
    // a loopback tap.
    client
        .initialize_client(&wavefmt, &Direction::Capture, &stream_mode)
        .map_err(|e| anyhow!("initialising \"{friendly}\" for {format}: {e}"))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|e| anyhow!("{e}"))?;
    client.start_stream().map_err(|e| anyhow!("{e}"))?;
    tracing::info!(device = %friendly, mode = ?mode, %format, "capturing Windows playback");

    let packet_samples = format.samples_per_packet();
    let channels = format.channels as usize;
    let frame_duration = Duration::from_millis(format.frame_ms as u64);
    // Four packets of slack; past that the endpoint is running ahead of our
    // schedule and the oldest audio is the least useful.
    let max_queue = packet_samples * 4;

    // Everything downstream is `i16`, and a byte queue could not be cast back
    // to one safely: `bytemuck` requires the alignment a `Vec<u8>` does not
    // promise. Staying in samples sidesteps that entirely.
    let mut queue: VecDeque<i16> = VecDeque::with_capacity(max_queue * 2);
    let mut scratch: Vec<i16> = Vec::new();
    let mut next_due = Instant::now() + frame_duration;

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            drain_endpoint(&capture, channels, &mut scratch, &mut queue)?;

            if queue.len() > max_queue {
                let excess = queue.len() - max_queue;
                queue.drain(..excess);
                tracing::debug!(
                    samples = excess,
                    "capture endpoint ran ahead; dropped oldest audio"
                );
            }

            if !emit_due_packets(
                &mut queue,
                packet_samples,
                frame_duration,
                &mut next_due,
                tx,
            ) {
                return Ok(()); // the network task is gone
            }
            thread::sleep(CAPTURE_POLL);
        }
        Ok(())
    })();

    let _ = client.stop_stream();
    result
}

/// Move every packet the endpoint has ready into `queue`. Buffers flagged
/// silent carry undefined bytes, so they are written as zeros rather than
/// copied.
fn drain_endpoint(
    capture: &wasapi::AudioCaptureClient,
    channels: usize,
    scratch: &mut Vec<i16>,
    queue: &mut VecDeque<i16>,
) -> Result<()> {
    loop {
        let available = capture.get_next_packet_size().map_err(|e| anyhow!("{e}"))?;
        let frames = match available {
            Some(n) if n > 0 => n as usize,
            _ => return Ok(()),
        };

        scratch.clear();
        scratch.resize(frames * channels, 0);
        let (read, info) = capture
            .read_from_device(bytemuck::cast_slice_mut::<i16, u8>(scratch))
            .map_err(|e| anyhow!("{e}"))?;

        let samples = read as usize * channels;
        if info.flags.silent {
            // A silent buffer's contents are undefined, not zeroed.
            queue.extend(std::iter::repeat_n(0, samples));
        } else {
            queue.extend(scratch[..samples].iter().copied());
        }
    }
}

/// Emit whole packets on a wall-clock schedule, substituting silence when the
/// endpoint has nothing to give.
///
/// A loopback tap simply goes quiet when no application is playing. If we went
/// quiet too, the host's jitter buffer would drain, underrun, and then have to
/// re-warm — adding its full buffer of latency to the first moment of every
/// sound. Sending silence instead keeps the pipeline primed for the cost of
/// 768 kbit/s of zeros on a virtual NIC.
///
/// Returns false once the receiving side has hung up.
fn emit_due_packets(
    queue: &mut VecDeque<i16>,
    packet_samples: usize,
    frame_duration: Duration,
    next_due: &mut Instant,
    tx: &tokio::sync::mpsc::Sender<Vec<i16>>,
) -> bool {
    let now = Instant::now();
    if now > *next_due + SCHEDULE_RESET {
        tracing::debug!("capture schedule slipped badly; resynchronising");
        *next_due = now;
    }

    while now >= *next_due {
        let pcm: Vec<i16> = if queue.len() >= packet_samples {
            queue.drain(..packet_samples).collect()
        } else {
            vec![0i16; packet_samples]
        };
        match tx.try_send(pcm) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!("network task is behind; dropped a playback packet");
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return false,
        }
        *next_due += frame_duration;
    }
    true
}

pub fn list_devices() -> Result<(Vec<DeviceInfo>, Vec<DeviceInfo>)> {
    init_com()?;
    let render = enumerate(Direction::Render)?;
    let capture = enumerate(Direction::Capture)?;
    Ok((render, capture))
}

fn enumerate(direction: Direction) -> Result<Vec<DeviceInfo>> {
    let enumerator = DeviceEnumerator::new().map_err(|e| anyhow!("{e}"))?;
    let default_id = enumerator
        .get_default_device(&direction)
        .ok()
        .and_then(|d| d.get_id().ok());

    let collection = enumerator
        .get_device_collection(&direction)
        .map_err(|e| anyhow!("listing {direction} devices: {e}"))?;
    let count = collection.get_nbr_devices().map_err(|e| anyhow!("{e}"))?;

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let device = collection
            .get_device_at_index(i)
            .map_err(|e| anyhow!("{e}"))?;
        let name = device.get_friendlyname().map_err(|e| anyhow!("{e}"))?;
        let id = device.get_id().ok();
        out.push(DeviceInfo {
            is_default: id.is_some() && id == default_id,
            name,
        });
    }
    Ok(out)
}
