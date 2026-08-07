//! Non-Windows placeholder so the crate still builds and its protocol and
//! configuration tests still run on a Linux workstation.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use audiobridge_proto::{Format, JitterBuffer};

use crate::audio::{AudioThread, DeviceInfo};
use crate::config::CaptureMode;

fn unsupported() -> anyhow::Error {
    anyhow::anyhow!("audiobridge-vm can only open audio devices on Windows")
}

pub fn list_devices() -> Result<(Vec<DeviceInfo>, Vec<DeviceInfo>)> {
    Err(unsupported())
}

pub fn spawn_render(
    _device: Option<String>,
    _format: Format,
    _jitter: Arc<Mutex<JitterBuffer>>,
) -> Result<AudioThread> {
    Err(unsupported())
}

pub fn spawn_capture(
    _device: Option<String>,
    _mode: CaptureMode,
    _format: Format,
    _tx: tokio::sync::mpsc::Sender<Vec<i16>>,
) -> Result<AudioThread> {
    Err(unsupported())
}
