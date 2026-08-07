//! PulseAudio-API access to the host's real headset.
//!
//! openSUSE Leap 16 runs PipeWire, but `pipewire-pulse` serves this API
//! faithfully, so the same binary works on a PipeWire or a classic PulseAudio
//! desktop and shows up in `pavucontrol` / `qpwgraph` as an ordinary
//! application. That is what we want: no virtual device on the Linux side, just
//! a client that records from the default source and plays to the default sink,
//! movable per-stream like any other app.
//!
//! Both directions run on dedicated blocking threads. `Simple::read` and
//! `Simple::write` block against the sound card's own clock, which is exactly
//! the pacing signal the jitter buffer's drift control needs.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use audiobridge_proto::{Format, JitterBuffer};
use libpulse_binding::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::error::PAErr;
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use libpulse_binding::proplist::{properties, Proplist};
use libpulse_binding::sample::{Format as PaFormat, Spec};
use libpulse_binding::stream::Direction as PaDirection;
use libpulse_simple_binding::Simple;

const APP_NAME: &str = "audiobridge";

/// How long a failed device open waits before trying again. Long enough not to
/// spin, short enough that plugging the headset back in feels immediate.
const REOPEN_DELAY: Duration = Duration::from_millis(1000);

/// One entry of `--list-devices`.
pub struct DeviceInfo {
    pub name: String,
    pub description: String,
    pub is_default: bool,
}

/// Handle to a running audio thread. Dropping it does not stop the thread;
/// call [`AudioThread::stop`].
pub struct AudioThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioThread {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // The thread can be parked inside a blocking PulseAudio call for up
            // to one buffer; joining is still bounded and keeps shutdown clean.
            let _ = h.join();
        }
    }
}

fn spec_of(format: &Format) -> Spec {
    Spec {
        // Native endianness: the wire format is little endian and
        // `crate::net` converts, so the device can speak whatever this CPU is.
        format: PaFormat::S16NE,
        rate: format.rate,
        channels: format.channels,
    }
}

fn pa(ctx: &str, e: PAErr) -> anyhow::Error {
    anyhow!("{ctx}: {e}")
}

/// Record from `device` (or the default source) and hand each packet to `tx`.
///
/// The channel is bounded: if the network task stalls, we would rather drop
/// fresh microphone audio than grow an unbounded backlog that arrives late.
pub fn spawn_capture(
    device: Option<String>,
    format: Format,
    tx: tokio::sync::mpsc::Sender<Vec<i16>>,
) -> AudioThread {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    let handle = thread::Builder::new()
        .name("pa-capture".into())
        .spawn(move || {
            // Read straight into an `i16` buffer and hand PulseAudio a byte
            // view of it. Casting that way only ever lowers the alignment
            // requirement; going from a `Vec<u8>` to `&[i16]` would not be
            // guaranteed to be aligned and `bytemuck` would refuse it.
            let mut pcm = vec![0i16; format.samples_per_packet()];
            let mut announced = false;

            while !thread_stop.load(Ordering::Relaxed) {
                let stream = match open(device.as_deref(), &format, PaDirection::Record) {
                    Ok(s) => s,
                    Err(e) => {
                        if !announced {
                            tracing::warn!("microphone unavailable: {e:#}");
                            announced = true;
                        }
                        thread::sleep(REOPEN_DELAY);
                        continue;
                    }
                };
                tracing::info!(device = device.as_deref().unwrap_or("(default)"), %format, "capturing");
                announced = false;

                while !thread_stop.load(Ordering::Relaxed) {
                    if let Err(e) = stream.read(bytemuck::cast_slice_mut::<i16, u8>(&mut pcm)) {
                        tracing::warn!("microphone read failed, reopening: {e}");
                        break;
                    }
                    match tx.try_send(pcm.clone()) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            tracing::debug!("network task is behind; dropped a microphone packet");
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
        })
        .expect("spawning the capture thread");

    AudioThread {
        stop,
        handle: Some(handle),
    }
}

/// Play whatever `jitter` hands out to `device` (or the default sink).
pub fn spawn_playback(
    device: Option<String>,
    format: Format,
    jitter: Arc<Mutex<JitterBuffer>>,
) -> AudioThread {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();

    let handle = thread::Builder::new()
        .name("pa-playback".into())
        .spawn(move || {
            let samples = format.samples_per_packet();
            let mut pcm = vec![0i16; samples];
            let mut announced = false;

            while !thread_stop.load(Ordering::Relaxed) {
                let stream = match open(device.as_deref(), &format, PaDirection::Playback) {
                    Ok(s) => s,
                    Err(e) => {
                        if !announced {
                            tracing::warn!("speakers unavailable: {e:#}");
                            announced = true;
                        }
                        thread::sleep(REOPEN_DELAY);
                        continue;
                    }
                };
                tracing::info!(device = device.as_deref().unwrap_or("(default)"), %format, "playing");
                announced = false;

                while !thread_stop.load(Ordering::Relaxed) {
                    // Hold the lock only for the copy, never across the
                    // blocking write below.
                    match jitter.lock() {
                        Ok(mut jb) => jb.pull(&mut pcm),
                        Err(_) => return, // a poisoned lock means the process is going down
                    }
                    if let Err(e) = stream.write(bytemuck::cast_slice::<i16, u8>(&pcm)) {
                        tracing::warn!("speaker write failed, reopening: {e}");
                        if let Ok(mut jb) = jitter.lock() {
                            jb.reset();
                        }
                        break;
                    }
                }
            }
        })
        .expect("spawning the playback thread");

    AudioThread {
        stop,
        handle: Some(handle),
    }
}

fn open(device: Option<&str>, format: &Format, dir: PaDirection) -> Result<Simple> {
    let spec = spec_of(format);
    if !spec.is_valid() {
        bail!("PulseAudio rejects {format} as a sample spec");
    }
    let bytes = format.payload_len() as u32;

    // `u32::MAX` is how libpulse spells "server default" in these fields.
    let attr = match dir {
        PaDirection::Record => BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            // One packet per read: the smallest fragment that still avoids a
            // syscall per sample.
            fragsize: bytes,
        },
        PaDirection::Playback => BufferAttr {
            maxlength: u32::MAX,
            // Three packets in flight on the device side. Less than this and
            // an ordinary scheduling hiccup underruns the card.
            tlength: bytes * 3,
            prebuf: bytes * 2,
            minreq: bytes,
            fragsize: u32::MAX,
        },
        _ => bail!("unsupported stream direction"),
    };

    let stream_name = match dir {
        PaDirection::Record => "microphone to VM",
        _ => "playback from VM",
    };

    Simple::new(
        None,
        APP_NAME,
        dir,
        device,
        stream_name,
        &spec,
        None,
        Some(&attr),
    )
    .map_err(|e| pa(&format!("opening {stream_name}"), e))
}

/// Enumerate sources and sinks, marking the current defaults.
pub fn list_devices() -> Result<(Vec<DeviceInfo>, Vec<DeviceInfo>)> {
    let mut proplist =
        Proplist::new().ok_or_else(|| anyhow!("allocating a PulseAudio proplist"))?;
    proplist
        .set_str(properties::APPLICATION_NAME, APP_NAME)
        .map_err(|_| anyhow!("setting the application name"))?;

    let mainloop = Rc::new(RefCell::new(
        Mainloop::new().ok_or_else(|| anyhow!("creating a PulseAudio mainloop"))?,
    ));
    let context = Rc::new(RefCell::new(
        Context::new_with_proplist(&*mainloop.borrow(), APP_NAME, &proplist)
            .ok_or_else(|| anyhow!("creating a PulseAudio context"))?,
    ));

    context
        .borrow_mut()
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .map_err(|e| pa("connecting to the sound server", e))
        .context("is PipeWire (or PulseAudio) running for this user?")?;

    // Pump the mainloop until the connection settles one way or the other.
    loop {
        iterate(&mainloop)?;
        match context.borrow().get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                bail!("the sound server refused the connection")
            }
            _ => {}
        }
    }

    let introspect = context.borrow().introspect();
    let (default_source, default_sink) = defaults(&mainloop, &introspect)?;
    let sources = collect(&mainloop, &introspect, Kind::Source, &default_source)?;
    let sinks = collect(&mainloop, &introspect, Kind::Sink, &default_sink)?;

    context.borrow_mut().disconnect();
    Ok((sources, sinks))
}

enum Kind {
    Source,
    Sink,
}

fn iterate(mainloop: &Rc<RefCell<Mainloop>>) -> Result<()> {
    match mainloop.borrow_mut().iterate(false) {
        IterateResult::Success(_) => Ok(()),
        IterateResult::Quit(_) => bail!("the PulseAudio mainloop quit"),
        IterateResult::Err(e) => Err(pa("iterating the PulseAudio mainloop", e)),
    }
}

fn defaults(
    mainloop: &Rc<RefCell<Mainloop>>,
    introspect: &libpulse_binding::context::introspect::Introspector,
) -> Result<(String, String)> {
    let slot: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));
    let done = Rc::new(Cell::new(false));

    let (slot_cb, done_cb) = (slot.clone(), done.clone());
    let op = introspect.get_server_info(move |info| {
        *slot_cb.borrow_mut() = Some((
            info.default_source_name
                .as_deref()
                .unwrap_or_default()
                .to_string(),
            info.default_sink_name
                .as_deref()
                .unwrap_or_default()
                .to_string(),
        ));
        done_cb.set(true);
    });

    while !done.get() {
        iterate(mainloop)?;
    }
    drop(op);

    let taken = slot.borrow_mut().take();
    taken.ok_or_else(|| anyhow!("the sound server did not report its defaults"))
}

fn collect(
    mainloop: &Rc<RefCell<Mainloop>>,
    introspect: &libpulse_binding::context::introspect::Introspector,
    kind: Kind,
    default_name: &str,
) -> Result<Vec<DeviceInfo>> {
    use libpulse_binding::callbacks::ListResult;

    let items: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let done = Rc::new(Cell::new(false));

    let (items_cb, done_cb) = (items.clone(), done.clone());
    let op: Box<dyn std::any::Any> = match kind {
        Kind::Source => Box::new(introspect.get_source_info_list(move |res| match res {
            ListResult::Item(i) => items_cb.borrow_mut().push((
                i.name.as_deref().unwrap_or_default().to_string(),
                i.description.as_deref().unwrap_or_default().to_string(),
            )),
            ListResult::End | ListResult::Error => done_cb.set(true),
        })),
        Kind::Sink => Box::new(introspect.get_sink_info_list(move |res| match res {
            ListResult::Item(i) => items_cb.borrow_mut().push((
                i.name.as_deref().unwrap_or_default().to_string(),
                i.description.as_deref().unwrap_or_default().to_string(),
            )),
            ListResult::End | ListResult::Error => done_cb.set(true),
        })),
    };

    while !done.get() {
        iterate(mainloop)?;
    }
    drop(op);

    let out = items
        .borrow()
        .iter()
        .map(|(name, description)| DeviceInfo {
            is_default: name == default_name,
            name: name.clone(),
            description: description.clone(),
        })
        .collect();
    Ok(out)
}
