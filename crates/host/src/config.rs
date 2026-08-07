use std::net::SocketAddr;
use std::path::Path;

use anyhow::{bail, Context, Result};
use audiobridge_proto::Format;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    pub server: ServerConfig,
    /// Host microphone -> VM virtual microphone.
    #[serde(default)]
    pub mic: MicConfig,
    /// VM playback -> host headphones.
    #[serde(default)]
    pub speaker: SpeakerConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `ip:port` of the VM agent.
    pub address: SocketAddr,
    #[serde(default)]
    pub token: String,
    /// Give up on a handshake attempt and retry after this long.
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_ms: u64,
    /// Declare the link dead and re-handshake after this long without a packet.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MicConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// PulseAudio/PipeWire source name. Empty means the system default.
    #[serde(default)]
    pub device: String,
    #[serde(default = "default_rate")]
    pub rate: u32,
    #[serde(default = "default_mono")]
    pub channels: u8,
    #[serde(default = "default_frame_ms")]
    pub frame_ms: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeakerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// PulseAudio/PipeWire sink name. Empty means the system default.
    #[serde(default)]
    pub device: String,
    #[serde(default = "default_rate")]
    pub rate: u32,
    #[serde(default = "default_stereo")]
    pub channels: u8,
    #[serde(default = "default_frame_ms")]
    pub frame_ms: u8,
    /// Jitter buffer target. This is the latency this side chooses to add in
    /// exchange for surviving network jitter and clock drift.
    #[serde(default = "default_buffer_ms")]
    pub buffer_ms: u32,
}

impl Default for MicConfig {
    fn default() -> Self {
        MicConfig {
            enabled: true,
            device: String::new(),
            rate: default_rate(),
            channels: default_mono(),
            frame_ms: default_frame_ms(),
        }
    }
}

impl Default for SpeakerConfig {
    fn default() -> Self {
        SpeakerConfig {
            enabled: true,
            device: String::new(),
            rate: default_rate(),
            channels: default_stereo(),
            frame_ms: default_frame_ms(),
            buffer_ms: default_buffer_ms(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_rate() -> u32 {
    48_000
}
fn default_mono() -> u8 {
    1
}
fn default_stereo() -> u8 {
    2
}
fn default_frame_ms() -> u8 {
    10
}
fn default_buffer_ms() -> u32 {
    40
}
fn default_handshake_timeout() -> u64 {
    1_000
}
fn default_idle_timeout() -> u64 {
    5_000
}

impl MicConfig {
    pub fn format(&self) -> Format {
        Format::new(self.rate, self.channels, self.frame_ms)
    }

    /// `None` means "let the server pick the default device".
    pub fn device(&self) -> Option<&str> {
        opt(&self.device)
    }
}

impl SpeakerConfig {
    pub fn format(&self) -> Format {
        Format::new(self.rate, self.channels, self.frame_ms)
    }

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

impl HostConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: HostConfig =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if !self.mic.enabled && !self.speaker.enabled {
            bail!("both `mic` and `speaker` are disabled: there is nothing to bridge");
        }

        for (label, format) in [
            ("mic", self.mic.format()),
            ("speaker", self.speaker.format()),
        ] {
            format
                .validate()
                .map_err(|e| anyhow::anyhow!("[{label}] {e}"))?;
            if !format.fits_ethernet_mtu() {
                tracing::warn!(
                    stream = label,
                    payload = format.payload_len(),
                    "a packet of this format needs IP fragmentation on a 1500-byte path; \
                     set frame_ms = 5 if the link is not a local virtual NIC"
                );
            }
        }

        if self.speaker.enabled {
            let packet_ms = self.speaker.frame_ms as u32;
            if self.speaker.buffer_ms < packet_ms * 2 {
                bail!(
                    "[speaker] buffer_ms ({}) must be at least two packets ({} ms)",
                    self.speaker.buffer_ms,
                    packet_ms * 2
                );
            }
        }

        if self.server.idle_timeout_ms < self.server.handshake_timeout_ms {
            bail!("server.idle_timeout_ms must not be shorter than handshake_timeout_ms");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<HostConfig> {
        let cfg: HostConfig = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn minimal_config_gets_sane_defaults() {
        let cfg = parse(
            r#"
            [server]
            address = "192.168.122.240:17420"
            "#,
        )
        .unwrap();

        assert!(cfg.mic.enabled && cfg.speaker.enabled);
        assert_eq!(cfg.mic.format(), Format::new(48_000, 1, 10));
        assert_eq!(cfg.speaker.format(), Format::new(48_000, 2, 10));
        assert_eq!(cfg.speaker.buffer_ms, 40);
        assert_eq!(cfg.mic.device(), None);
    }

    #[test]
    fn rejects_a_bridge_with_both_directions_off() {
        let err = parse(
            r#"
            [server]
            address = "127.0.0.1:1"
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
    fn rejects_a_jitter_buffer_shorter_than_two_packets() {
        let err = parse(
            r#"
            [server]
            address = "127.0.0.1:1"
            [speaker]
            frame_ms = 20
            buffer_ms = 30
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("at least two packets"));
    }

    #[test]
    fn rejects_an_unsupported_format() {
        let err = parse(
            r#"
            [server]
            address = "127.0.0.1:1"
            [mic]
            channels = 6
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("[mic]"));
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = parse(
            r#"
            [server]
            address = "127.0.0.1:1"
            tokne = "typo"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("tokne"));
    }
}
