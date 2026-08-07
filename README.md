# audio-bridge

Use the headset plugged into a Linux machine from a Windows VM: your voice
reaches the guest as a microphone, and whatever the guest plays comes back to
your headphones. Two small binaries and a UDP link between them.

```
 Linux (audiobridge-host)                    Windows guest (audiobridge-vm)
 ┌──────────────────────┐                    ┌─────────────────────────────┐
 │ headset mic  ──► PA  │ ══ mic packets ══► │ ──► "CABLE Input" (render)  │
 │                      │                    │       ↳ apps record from    │
 │                      │                    │         "CABLE Output"      │
 │ headphones ◄── PA    │ ◄═ spk packets ══  │ ◄── loopback on a playback  │
 │                      │                    │     device that is not that │
 │                      │                    │     cable (see below)       │
 └──────────────────────┘                    └─────────────────────────────┘
```

Uncompressed 16-bit PCM over UDP with a jitter buffer at each receiver. On a
virtio link that costs a few Mbit/s and buys you no codec latency and no codec
artifacts.

## Getting the binaries

### From a release

Pushing a tag builds both sides and publishes them, so the usual answer is to
take the archives from the
[releases page](https://github.com/capric8416/audio-bridge/releases) rather than
build anything. Each one carries its binary, its config template and the README.

- `audiobridge-<tag>-linux-x64.tar.gz` — the host binary plus `host.toml` and
  the systemd unit. Built on Debian 11, so it needs glibc 2.30 or newer
  (Debian 11+, Ubuntu 20.04+) and PulseAudio or PipeWire already installed.
- `audiobridge-<tag>-windows-x64.zip` — `audiobridge-vm.exe` plus `vm.toml`.

### From source

The two binaries are built for different machines, from the same workspace.

```sh
# Linux side
cargo build --release -p audiobridge-host

# Windows side, cross-compiled from Linux
rustup target add x86_64-pc-windows-msvc
cargo xwin build --release -p audiobridge-vm --target x86_64-pc-windows-msvc
```

`audiobridge-host` links against PulseAudio, so it needs `libpulse-dev`
(Debian/Ubuntu) or `pulseaudio-libs-devel` (Fedora). It talks to PipeWire fine
through PipeWire's PulseAudio interface.

Building the guest binary on Windows itself is just
`cargo build --release -p audiobridge-vm`.

## Setting it up

### 1. Give the guest a virtual microphone

Windows has no way to invent a recording device, so the guest needs a virtual
audio cable — [VB-CABLE](https://vb-audio.com/Cable/) is the usual free choice.
Install it in the guest and reboot. You get a pair of devices: `CABLE Input`
(a playback device) and `CABLE Output` (a recording device). This bridge plays
your voice into `CABLE Input`; Teams, Discord and friends select `CABLE Output`
as their microphone.

### 1b. Give the guest somewhere to play

The return direction taps a playback device with `loopback`, and that device
must not be the cable from the previous step. If it is — which is what happens
when the cable is the guest's only playback device, since installing VB-CABLE
also makes it the default — the bridge loops your own voice straight back to
your headphones and mixes it into what applications record.

A VM often has no emulated sound card at all, so check with `--list-devices`.
If the render list contains nothing but `CABLE …`, add one; under libvirt that
is one line in `virsh edit`, inside `<devices>`:

```xml
<sound model='ich9'/>
```

Windows then gains `Speakers (High Definition Audio Device)`. Make it the
default output so applications play there, and point `[speaker].device` at it.
Nothing has to come out of it on the Linux side — the loopback tap works even
when the host discards the QEMU audio backend's output.

The alternative is a second virtual cable (VB-Audio Cable A/B): make
`CABLE-A Input` the default output and either loopback it or read `CABLE-A
Output` directly with `mode = "capture"`.

### 2. Configure both sides

Copy `config/host.toml` and `config/vm.toml` and edit them. The two things that
must agree are `token` and the address/port; the audio format is proposed by the
host during the handshake and the guest follows, so it cannot drift.

List the device names on each machine:

```sh
audiobridge-host --list-devices     # PulseAudio sources and sinks
audiobridge-vm.exe --list-devices   # WASAPI render and capture endpoints
```

Copy the name you want into the config verbatim, then check it parses:

```sh
audiobridge-host --config host.toml --check
```

### 3. Open the port

The guest listens on UDP 17420 by default. Windows Firewall blocks it until told
otherwise:

```powershell
New-NetFirewallRule -DisplayName "audiobridge" -Direction Inbound `
  -Protocol UDP -LocalPort 17420 -Action Allow
```

### 4. Run them

Start the guest side first — it waits for a handshake; the host retries until it
gets one, so the order only affects how long the first connection takes.

```powershell
audiobridge-vm.exe --config vm.toml
```

```sh
audiobridge-host --config host.toml
```

The host logs a line per session, and one line per stream with loss and buffer
statistics every 10 seconds.

## Running it in the background

On Linux, as a user service — it has to be a *user* service, because the
PulseAudio/PipeWire socket belongs to your login session:

```sh
mkdir -p ~/.config/systemd/user
cp dist/audiobridge-host.service ~/.config/systemd/user/
mkdir -p ~/.config/audiobridge && cp config/host.toml ~/.config/audiobridge/
systemctl --user enable --now audiobridge-host
journalctl --user -u audiobridge-host -f
```

On Windows, the simplest reliable option is a Task Scheduler entry set to run at
logon. Do not run it as a Windows service: services live in session 0 and cannot
reach the audio endpoints.

## Tuning

`frame_ms` sets how much audio goes in each packet and is the floor on latency.
`buffer_ms` on each receiver sets how much jitter it can absorb before you hear
a gap.

- **Local VM on a virtio NIC** — `frame_ms = 10`, `buffer_ms = 40`. Comfortable.
- **Chasing latency** — `frame_ms = 5`, `buffer_ms = 20`. Twice the packet rate,
  around 30 ms end to end.
- **Over Wi-Fi** — `frame_ms = 5` and `buffer_ms = 60` or more. Use 5 ms here
  even though it costs packet rate: 10 ms stereo at 48 kHz is 1920 bytes, which
  fragments on a 1500-byte path, and a fragmented packet is lost whole. The host
  warns at startup when the configured format has this problem.

If the log reports underruns, raise `buffer_ms` on the side reporting them. If
it reports loss, the network is dropping packets and a bigger buffer will not
help.

Mono for the microphone and stereo for playback is the sensible default; sending
a mono headset mic as stereo just doubles the bandwidth.

## How it works

`crates/proto` holds the wire format and the jitter buffer, and is shared by
both binaries. Packets are a 12-byte header plus little-endian interleaved
samples, on one UDP socket per session, with sequence numbers per direction.

The receiver's jitter buffer is a ring sized from the configured latency. A
missing packet is filled by fading out the last audio rather than inserting a
click of silence; packets that arrive after their slot has played are dropped
rather than reordered, which trades a rare artifact for latency that would
otherwise have to be paid on every packet. A sequence jump too large to be
plausible jitter resynchronises the buffer instead of concealing thousands of
frames.

Authentication is a shared token compared in constant time, and a failed
handshake is answered with a bare `AuthFailed` and no detail. This is meant for
a virtual network you already trust — the audio itself is not encrypted.

## Testing

```sh
cargo test --workspace
```

`crates/proto/tests/session.rs` runs a handshake over real UDP sockets, pushes a
full-size fragmenting packet across the wire, and drives a stream through the
jitter buffer while dropping, reordering and duplicating packets. The audio
device layers are the part no test covers; they need real hardware.

## Cutting a release

`.github/workflows/release.yml` fires on any pushed tag. It gates on
`cargo fmt --check`, `cargo clippy -D warnings` and the test suite before it
builds anything, then publishes both archives with generated release notes.

```sh
git tag v0.1.0
git push origin v0.1.0
```

The Linux job runs inside a `debian:bullseye` container rather than on the
runner image. `audiobridge-host` links libpulse, so it cannot be built with
cargo-zigbuild the way a pure-Rust binary can — building against an old
distribution's libpulse and glibc is the reliable way to get a portable binary.
