//! Windows audio devices, behind a backend-neutral surface.
//!
//! The real implementation is WASAPI. A stub keeps the crate compiling (and its
//! configuration and protocol tests runnable) on Linux, which is handy when
//! iterating on the wire format without a Windows box in reach.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

#[cfg(windows)]
#[path = "audio_wasapi.rs"]
mod backend;

#[cfg(not(windows))]
#[path = "audio_stub.rs"]
mod backend;

pub use backend::{list_devices, spawn_capture, spawn_render};

/// One entry of `--list-devices`.
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
}

/// Handle to a running audio thread.
pub struct AudioThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioThread {
    /// Only the WASAPI backend builds these; off Windows nothing calls it.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn new(stop: Arc<AtomicBool>, handle: JoinHandle<()>) -> Self {
        AudioThread {
            stop,
            handle: Some(handle),
        }
    }

    /// Ask the thread to finish and wait for it. Bounded by the backend's
    /// event-wait timeout, so this returns promptly even mid-stream.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
