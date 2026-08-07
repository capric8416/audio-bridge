//! Receive-side buffering for a one-way PCM stream.
//!
//! Two problems have to be solved at once, and they pull in opposite
//! directions:
//!
//! * **Jitter.** Packets leave the sender every `frame_ms` but arrive in
//!   clumps. A buffer absorbs that, at the cost of latency.
//! * **Clock drift.** The capturing and the playing sound card each have their
//!   own crystal. Even at a matched nominal 48 kHz they differ by tens of parts
//!   per million, so one side produces slightly more samples per second than
//!   the other consumes. Left alone the buffer creeps towards empty or towards
//!   full and eventually clicks — once every few minutes, forever.
//!
//! The fix for drift is to consume samples at a rate that tracks the buffer
//! level instead of at exactly 1.0. [`JitterBuffer::pull`] reads through a
//! linear interpolator whose step size is nudged by how far the fill level sits
//! from its target. The correction is clamped to [`MAX_RATE_DEVIATION`], which
//! is far above any real crystal error and far below what an ear can hear as a
//! pitch change.

use std::collections::VecDeque;

/// Largest playback-rate correction applied to chase clock drift, as a
/// fraction. 0.4 % is ~7 cents of pitch — inaudible — and roughly 20x the
/// worst-case error between two consumer crystals.
pub const MAX_RATE_DEVIATION: f64 = 0.004;

/// Proportional gain from normalised fill error to rate correction.
const DRIFT_GAIN: f64 = 0.01;

/// Per-pull smoothing of the rate. At 100 pulls/s this is a ~0.5 s time
/// constant, so the rate never steps abruptly.
const RATE_SMOOTHING: f64 = 0.02;

/// A gap larger than this many packets is treated as a stream restart rather
/// than as loss worth concealing.
const RESYNC_GAP_PACKETS: u32 = 50;

/// Frames over which a concealed or underrun region fades to silence.
const FADE_FRAMES: usize = 240;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    /// Audio packets accepted into the buffer.
    pub accepted: u64,
    /// Packets that arrived after their slot had already been played.
    pub late: u64,
    /// Packets the sequence numbers say never showed up.
    pub lost: u64,
    /// `pull` calls that ran out of samples.
    pub underruns: u64,
    /// Samples discarded because the buffer grew past its hard cap.
    pub overruns: u64,
    /// Times the stream jumped far enough to force a cold restart.
    pub resyncs: u64,
}

pub struct JitterBuffer {
    channels: usize,
    frames_per_packet: usize,
    /// Fill level the rate controller aims for, in frames.
    target_frames: usize,
    /// Hard cap; past this the oldest audio is dropped.
    max_frames: usize,

    queue: VecDeque<i16>,
    /// Last input frame consumed by the interpolator, held for the next lerp
    /// and reused as concealment material.
    prev: Vec<i16>,
    /// Interpolator position between `prev` and the head of `queue`.
    frac: f64,
    rate: f64,

    /// True until the buffer has filled to target; `pull` returns silence.
    warming: bool,
    expected_seq: Option<u32>,
    stats: JitterStats,
}

impl JitterBuffer {
    /// `target_ms` is the steady-state latency contributed by this buffer.
    /// It is rounded up to at least two packets, since one packet is always
    /// held back as interpolator lookahead.
    pub fn new(channels: usize, rate_hz: u32, frame_ms: u8, target_ms: u32) -> Self {
        assert!(channels >= 1, "a stream needs at least one channel");
        let frames_per_packet = (rate_hz as usize * frame_ms as usize) / 1000;
        let frames_per_packet = frames_per_packet.max(1);
        let target_frames =
            ((rate_hz as usize * target_ms as usize) / 1000).max(frames_per_packet * 2);
        let max_frames = (target_frames * 4).max(frames_per_packet * 8);

        JitterBuffer {
            channels,
            frames_per_packet,
            target_frames,
            max_frames,
            queue: VecDeque::with_capacity(max_frames * channels),
            prev: vec![0; channels],
            frac: 0.0,
            rate: 1.0,
            warming: true,
            expected_seq: None,
            stats: JitterStats::default(),
        }
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    pub fn fill_frames(&self) -> usize {
        self.queue.len() / self.channels
    }

    pub fn target_frames(&self) -> usize {
        self.target_frames
    }

    /// Current playback rate correction, for logging. 1.0 means "no drift".
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Drop everything and wait for the buffer to refill before playing again.
    pub fn reset(&mut self) {
        self.queue.clear();
        self.prev.iter_mut().for_each(|s| *s = 0);
        self.frac = 0.0;
        self.rate = 1.0;
        self.warming = true;
        self.expected_seq = None;
    }

    /// Accept one audio packet. `samples` is interleaved and must be a whole
    /// number of frames; a partial trailing frame is ignored.
    pub fn push(&mut self, seq: u32, samples: &[i16]) {
        match self.expected_seq {
            None => {
                self.expected_seq = Some(seq.wrapping_add(1));
            }
            Some(expected) => {
                // Wrapping difference: correct for any real gap, and treats a
                // sequence-space wrap as the small forward step it is.
                let delta = seq.wrapping_sub(expected) as i32;
                if delta < 0 {
                    // Already played through this slot, or a duplicate.
                    self.stats.late += 1;
                    return;
                }
                if delta as u32 > RESYNC_GAP_PACKETS {
                    self.stats.resyncs += 1;
                    self.reset();
                    self.expected_seq = Some(seq.wrapping_add(1));
                } else {
                    if delta > 0 {
                        self.stats.lost += delta as u64;
                        self.conceal(delta as usize);
                    }
                    self.expected_seq = Some(seq.wrapping_add(1));
                }
            }
        }

        let usable = samples.len() - (samples.len() % self.channels);
        self.queue.extend(&samples[..usable]);
        self.stats.accepted += 1;
        self.trim();
    }

    /// Synthesise `packets` worth of filler for a detected gap: the last good
    /// frame, faded out. Short gaps stay inaudible; long ones go quiet instead
    /// of buzzing.
    fn conceal(&mut self, packets: usize) {
        let frames = packets.saturating_mul(self.frames_per_packet);
        let base: Vec<i16> = self.last_frame().to_vec();
        for i in 0..frames {
            let gain = fade_out(i);
            for &s in &base {
                self.queue.push_back(scale(s, gain));
            }
            if gain == 0.0 {
                // The rest of the gap is silence; push it without recomputing.
                for _ in i + 1..frames {
                    for _ in 0..self.channels {
                        self.queue.push_back(0);
                    }
                }
                break;
            }
        }
    }

    /// The most recent frame available as concealment material: the tail of the
    /// queue if there is one, otherwise the interpolator's last input.
    fn last_frame(&self) -> &[i16] {
        let len = self.queue.len();
        if len >= self.channels {
            // `VecDeque::as_slices` may split the tail, so fall back to `prev`
            // when the last frame straddles the seam.
            let (a, b) = self.queue.as_slices();
            if b.len() >= self.channels {
                return &b[b.len() - self.channels..];
            }
            if b.is_empty() && a.len() >= self.channels {
                return &a[a.len() - self.channels..];
            }
        }
        &self.prev
    }

    fn trim(&mut self) {
        let excess = self.fill_frames().saturating_sub(self.max_frames);
        if excess == 0 {
            return;
        }
        // Drop back to target rather than to the cap, so this does not fire
        // again on the very next packet.
        let drop_frames = excess + self.max_frames.saturating_sub(self.target_frames);
        let drop_frames = drop_frames.min(self.fill_frames());
        self.queue.drain(..drop_frames * self.channels);
        self.stats.overruns += (drop_frames * self.channels) as u64;
    }

    /// Fill `out` (interleaved, a whole number of frames) with the next slice
    /// of audio. Always writes every sample: silence while warming up, faded
    /// concealment on underrun.
    pub fn pull(&mut self, out: &mut [i16]) {
        let frames = out.len() / self.channels;

        if self.warming {
            if self.fill_frames() < self.target_frames {
                out.iter_mut().for_each(|s| *s = 0);
                return;
            }
            self.warming = false;
            self.frac = 0.0;
            self.rate = 1.0;
        }

        self.update_rate();

        for f in 0..frames {
            // Advance past whole input frames the interpolator has left behind.
            while self.frac >= 1.0 {
                if self.queue.len() < self.channels {
                    self.underrun(&mut out[f * self.channels..]);
                    return;
                }
                for c in 0..self.channels {
                    self.prev[c] = self.queue.pop_front().unwrap();
                }
                self.frac -= 1.0;
            }
            // One frame of lookahead is needed to interpolate towards.
            if self.queue.len() < self.channels {
                self.underrun(&mut out[f * self.channels..]);
                return;
            }

            let t = self.frac;
            for c in 0..self.channels {
                let a = self.prev[c] as f64;
                let b = self.queue[c] as f64;
                out[f * self.channels + c] = (a + (b - a) * t).round() as i16;
            }
            self.frac += self.rate;
        }
    }

    /// Nudge the read rate towards whatever keeps the fill level at target.
    /// Reading faster than 1.0 drains a buffer that drift is filling up.
    fn update_rate(&mut self) {
        let fill = self.fill_frames() as f64;
        let target = self.target_frames as f64;
        let error = (fill - target) / target;
        let desired =
            (1.0 + error * DRIFT_GAIN).clamp(1.0 - MAX_RATE_DEVIATION, 1.0 + MAX_RATE_DEVIATION);
        self.rate += (desired - self.rate) * RATE_SMOOTHING;
    }

    /// Ran dry mid-pull: fade the last frame out over the remainder and go back
    /// to warming up so the next pull does not stutter one frame at a time.
    fn underrun(&mut self, rest: &mut [i16]) {
        self.stats.underruns += 1;
        let base: Vec<i16> = self.prev.clone();
        for (i, chunk) in rest.chunks_mut(self.channels).enumerate() {
            let gain = fade_out(i);
            for (c, slot) in chunk.iter_mut().enumerate() {
                *slot = scale(base[c], gain);
            }
        }
        self.queue.clear();
        self.prev.iter_mut().for_each(|s| *s = 0);
        self.frac = 0.0;
        self.warming = true;
    }
}

fn fade_out(i: usize) -> f64 {
    if i >= FADE_FRAMES {
        0.0
    } else {
        1.0 - (i as f64 / FADE_FRAMES as f64)
    }
}

fn scale(sample: i16, gain: f64) -> i16 {
    (sample as f64 * gain).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: i16, len: usize) -> Vec<i16> {
        (0..len).map(|i| start.wrapping_add(i as i16)).collect()
    }

    /// 48 kHz mono, 10 ms packets (480 frames), 30 ms target (1440 frames).
    fn mono() -> JitterBuffer {
        JitterBuffer::new(1, 48_000, 10, 30)
    }

    #[test]
    fn silence_until_warmed_up() {
        let mut jb = mono();
        let mut out = vec![7i16; 480];
        jb.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0), "must not play before prefill");

        for seq in 0..2 {
            jb.push(seq, &ramp(0, 480));
        }
        let mut out = vec![7i16; 480];
        jb.pull(&mut out);
        assert!(
            out.iter().all(|&s| s == 0),
            "960 frames is still below the 1440 target"
        );
    }

    #[test]
    fn passes_audio_through_once_warm() {
        let mut jb = mono();
        let mut sent = Vec::new();
        for seq in 0..4 {
            let pkt = ramp((seq * 480) as i16, 480);
            sent.extend_from_slice(&pkt);
            jb.push(seq, &pkt);
        }

        let mut out = vec![0i16; 480];
        jb.pull(&mut out);
        // The interpolator holds one frame back, so output lags input by one
        // sample and starts from its zeroed history.
        assert_eq!(out[0], 0);
        assert_eq!(&out[1..], &sent[..479]);
        assert_eq!(jb.stats().underruns, 0);
    }

    #[test]
    fn counts_loss_and_conceals_the_gap() {
        let mut jb = mono();
        jb.push(0, &ramp(0, 480));
        jb.push(1, &ramp(100, 480));
        jb.push(4, &ramp(200, 480)); // packets 2 and 3 never arrived

        let s = jb.stats();
        assert_eq!(s.lost, 2);
        assert_eq!(s.accepted, 3);
        // Three real packets plus two concealed ones.
        assert_eq!(jb.fill_frames(), 480 * 5);
    }

    #[test]
    fn drops_late_and_duplicate_packets() {
        let mut jb = mono();
        jb.push(0, &ramp(0, 480));
        jb.push(1, &ramp(0, 480));
        jb.push(1, &ramp(0, 480)); // duplicate
        jb.push(0, &ramp(0, 480)); // reordered, already past

        assert_eq!(jb.stats().late, 2);
        assert_eq!(jb.stats().accepted, 2);
        assert_eq!(jb.fill_frames(), 960);
    }

    #[test]
    fn sequence_wraparound_is_not_loss() {
        let mut jb = mono();
        jb.push(u32::MAX - 1, &ramp(0, 480));
        jb.push(u32::MAX, &ramp(0, 480));
        jb.push(0, &ramp(0, 480));
        jb.push(1, &ramp(0, 480));

        let s = jb.stats();
        assert_eq!(s.lost, 0, "wrapping past u32::MAX is a normal +1 step");
        assert_eq!(s.late, 0);
        assert_eq!(s.resyncs, 0);
        assert_eq!(s.accepted, 4);
    }

    #[test]
    fn huge_jump_resyncs_instead_of_concealing() {
        let mut jb = mono();
        jb.push(0, &ramp(0, 480));
        jb.push(10_000, &ramp(0, 480));

        assert_eq!(jb.stats().resyncs, 1);
        assert_eq!(
            jb.fill_frames(),
            480,
            "buffer restarted from the new packet"
        );
    }

    #[test]
    fn caps_the_buffer_when_the_sender_runs_ahead() {
        let mut jb = mono();
        for seq in 0..100 {
            jb.push(seq, &ramp(0, 480));
        }
        assert!(jb.stats().overruns > 0);
        assert!(
            jb.fill_frames() <= jb.target_frames() * 4,
            "fill {} exceeded the cap",
            jb.fill_frames()
        );
    }

    #[test]
    fn underrun_fades_out_and_rewarms() {
        let mut jb = mono();
        for seq in 0..4 {
            jb.push(seq, &vec![10_000i16; 480]);
        }
        let mut out = vec![0i16; 480 * 5];
        jb.pull(&mut out); // asks for more than the 1920 buffered frames

        assert_eq!(jb.stats().underruns, 1);
        assert_eq!(
            *out.last().unwrap(),
            0,
            "tail must fade to silence, not click"
        );

        // Back to warming: the next pull is silent until the buffer refills.
        let mut out = vec![7i16; 480];
        jb.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0));
    }

    /// A sender whose clock runs 0.2 % fast must not overflow the buffer: the
    /// rate controller should speed the reader up to match.
    #[test]
    fn tracks_a_fast_sender_without_overrunning() {
        let mut jb = mono();
        let mut seq = 0u32;
        let mut out = vec![0i16; 480];
        let mut pending = 0.0f64;

        for _ in 0..3000 {
            // 480.96 frames produced per 480-frame pull.
            pending += 480.96;
            while pending >= 480.0 {
                jb.push(seq, &vec![0i16; 480]);
                seq = seq.wrapping_add(1);
                pending -= 480.0;
            }
            jb.pull(&mut out);
        }

        let s = jb.stats();
        assert_eq!(s.overruns, 0, "rate control should have absorbed the drift");
        assert_eq!(s.underruns, 0);
        assert!(
            jb.rate() > 1.0,
            "reader must run fast to keep up, got {}",
            jb.rate()
        );
        assert!(jb.rate() <= 1.0 + MAX_RATE_DEVIATION + 1e-9);
    }

    /// The mirror image: a sender 0.2 % slow must not starve the reader.
    #[test]
    fn tracks_a_slow_sender_without_underrunning() {
        let mut jb = mono();
        let mut seq = 0u32;
        let mut out = vec![0i16; 480];
        let mut pending = 0.0f64;

        // Prefill so the first pulls are not warming-silence.
        for _ in 0..4 {
            jb.push(seq, &vec![0i16; 480]);
            seq = seq.wrapping_add(1);
        }

        for _ in 0..3000 {
            pending += 479.04;
            while pending >= 480.0 {
                jb.push(seq, &vec![0i16; 480]);
                seq = seq.wrapping_add(1);
                pending -= 480.0;
            }
            jb.pull(&mut out);
        }

        let s = jb.stats();
        assert_eq!(
            s.underruns, 0,
            "rate control should have absorbed the drift"
        );
        assert!(
            jb.rate() < 1.0,
            "reader must run slow to keep up, got {}",
            jb.rate()
        );
        assert!(jb.rate() >= 1.0 - MAX_RATE_DEVIATION - 1e-9);
    }

    #[test]
    fn stereo_frames_stay_aligned() {
        let mut jb = JitterBuffer::new(2, 48_000, 10, 30);
        // Left channel is +1000, right is -1000, forever.
        for seq in 0..8 {
            let pkt: Vec<i16> = (0..960)
                .map(|i| if i % 2 == 0 { 1000 } else { -1000 })
                .collect();
            jb.push(seq, &pkt);
        }
        let mut out = vec![0i16; 960];
        jb.pull(&mut out);
        // Skip the first frame, which interpolates out of zeroed history.
        for chunk in out[2..].chunks_exact(2) {
            assert_eq!(chunk[0], 1000);
            assert_eq!(chunk[1], -1000);
        }
    }

    #[test]
    fn partial_trailing_frame_is_ignored() {
        let mut jb = JitterBuffer::new(2, 48_000, 10, 30);
        jb.push(0, &[1, 2, 3]); // one and a half stereo frames
        assert_eq!(jb.fill_frames(), 1);
    }
}
