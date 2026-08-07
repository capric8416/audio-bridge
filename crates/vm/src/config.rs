use std::net::SocketAddr;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmConfig {
    /// `ip:port` to listen on. `0.0.0.0:17322` accepts from any interface.
    pub listen: SocketAddr,
    #[serde(default)]
    pub token: String,
    /// Tear the session down after this long without a packet from the host.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_ms: u64,
    /// Where audio arriving from Linux is played: the render half of a virtual
    /// cable, whose capture half applications then select as their microphone.
    #[serde(default)]
    pub mic: MicConfig,
    /// What is captured and sent back to Linux.
    #[serde(default)]
    pub speaker: SpeakerConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Friendly name of a *render* endpoint, e.g.
    /// `CABLE Input (VB-Audio Virtual Cable)`. Empty means the default one,
    /// which is almost never what you want here.
    #[serde(default)]
    pub device: String,
    /// Jitter buffer target for the stream arriving from Linux.
    #[serde(default = "default_buffer_ms")]
    pub buffer_ms: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: CaptureMode,
    /// Friendly name of the endpoint to take audio from. Which kind of endpoint
    /// depends on `mode`. Empty means that mode's default endpoint.
    #[serde(default)]
    pub device: String,
}

/// How the Windows side gets hold of the audio applications are playing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    /// Tap a *render* endpoint with `AUDCLNT_STREAMFLAGS_LOOPBACK`. Point it at
    /// whatever the system default playback device is and everything Windows
    /// plays comes back, no second virtual cable required.
    #[default]
    Loopback,
    /// Read a real *capture* endpoint. Use this when playback is routed into a
    /// second virtual cable, whose capture half is then read directly — one
    /// less conversion than loopback, and it keeps working when the render
    /// endpoint goes idle.
    Capture,
}

impl Default for MicConfig {
    fn default() -> Self {
        MicConfig {
            enabled: true,
            device: String::new(),
            buffer_ms: default_buffer_ms(),
        }
    }
}

impl Default for SpeakerConfig {
    fn default() -> Self {
        SpeakerConfig {
            enabled: true,
            mode: CaptureMode::default(),
            device: String::new(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_buffer_ms() -> u32 {
    40
}
fn default_idle_timeout() -> u64 {
    5_000
}

impl MicConfig {
    pub fn device(&self) -> Option<&str> {
        opt(&self.device)
    }
}

impl SpeakerConfig {
    pub fn device(&self) -> Option<&str> {
        opt(&self.device)
    }
}

fn opt(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

impl VmConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: VmConfig =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if !self.mic.enabled && !self.speaker.enabled {
            bail!("both `mic` and `speaker` are disabled: there is nothing to bridge");
        }
        if self.mic.enabled && self.mic.buffer_ms < 10 {
            bail!(
                "[mic] buffer_ms of {} is too small to absorb any jitter",
                self.mic.buffer_ms
            );
        }
        if self.mic.enabled && self.mic.device().is_none() {
            tracing::warn!(
                "[mic].device is empty, so audio from Linux goes to the default playback \
                 device — set it to the render half of a virtual cable (e.g. \
                 \"CABLE Input (VB-Audio Virtual Cable)\") or Windows applications will \
                 hear it instead of recording it"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<VmConfig> {
        let cfg: VmConfig = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn minimal_config_gets_sane_defaults() {
        let cfg = parse(
            r#"
            listen = "0.0.0.0:17322"
            [mic]
            device = "CABLE Input (VB-Audio Virtual Cable)"
            "#,
        )
        .unwrap();

        assert!(cfg.mic.enabled && cfg.speaker.enabled);
        assert_eq!(cfg.mic.buffer_ms, 40);
        assert_eq!(cfg.speaker.mode, CaptureMode::Loopback);
        assert_eq!(cfg.speaker.device(), None);
        assert_eq!(cfg.idle_timeout_ms, 5_000);
    }

    #[test]
    fn capture_mode_parses_from_a_bare_word() {
        let cfg = parse(
            r#"
            listen = "0.0.0.0:17322"
            [mic]
            device = "CABLE Input"
            [speaker]
            mode = "capture"
            device = "CABLE-A Output (VB-Audio Cable A)"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.speaker.mode, CaptureMode::Capture);
        assert_eq!(
            cfg.speaker.device(),
            Some("CABLE-A Output (VB-Audio Cable A)")
        );
    }

    #[test]
    fn rejects_a_bridge_with_both_directions_off() {
        let err = parse(
            r#"
            listen = "0.0.0.0:1"
            [mic]
            enabled = false
            [speaker]
            enabled = false
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("nothing to bridge"));
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = parse(
            r#"
            listen = "0.0.0.0:1"
            [speaker]
            moed = "loopback"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("moed"));
    }

    #[test]
    fn rejects_an_unknown_capture_mode() {
        assert!(parse(
            r#"
            listen = "0.0.0.0:1"
            [speaker]
            mode = "magic"
            "#,
        )
        .is_err());
    }
}
