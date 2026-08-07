//! End-to-end checks of the wire format: a real handshake over real UDP
//! sockets, full-size media packets on the wire, and a media stream carried
//! through the jitter buffer while the network misbehaves.

use std::net::UdpSocket;
use std::time::Duration;

use audiobridge_proto as proto;
use proto::{Direction, Format, Incoming, JitterBuffer, Packet, Status};

const TOKEN: &str = "s3cret-token";

fn mic_format() -> Format {
    Format::new(48_000, 1, 10)
}

fn speaker_format() -> Format {
    Format::new(48_000, 2, 10)
}

fn pair() -> (UdpSocket, UdpSocket) {
    let host = UdpSocket::bind("127.0.0.1:0").expect("binding the host socket");
    let vm = UdpSocket::bind("127.0.0.1:0").expect("binding the VM socket");
    host.connect(vm.local_addr().unwrap()).unwrap();
    for s in [&host, &vm] {
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    }
    (host, vm)
}

#[test]
fn handshake_round_trips_over_udp() {
    let (host, vm) = pair();
    let session = 0x1234_5678;

    host.send(
        &Packet::Hello {
            token: TOKEN.as_bytes().to_vec(),
            session,
            mic: mic_format(),
            speaker: speaker_format(),
        }
        .encode(),
    )
    .unwrap();

    // VM side: read it, check the token, answer.
    let mut buf = vec![0u8; proto::MAX_PACKET];
    let (n, peer) = vm.recv_from(&mut buf).unwrap();
    let ack = match proto::decode(&buf[..n]).unwrap() {
        Incoming::Control(Packet::Hello {
            token,
            session: id,
            mic,
            speaker,
        }) => {
            assert!(proto::token_matches(TOKEN, &token));
            assert_eq!(id, session);
            assert_eq!(mic, mic_format());
            assert_eq!(speaker, speaker_format());
            Packet::HelloAck {
                status: Status::Ok,
                session: id,
                mic,
                speaker,
                message: "mic -> CABLE Input".to_string(),
            }
        }
        other => panic!("expected a Hello, got {other:?}"),
    };
    vm.send_to(&ack.encode(), peer).unwrap();

    let n = host.recv(&mut buf).unwrap();
    match proto::decode(&buf[..n]).unwrap() {
        Incoming::Control(Packet::HelloAck {
            status,
            session: id,
            message,
            ..
        }) => {
            assert_eq!(status, Status::Ok);
            assert_eq!(id, session);
            assert_eq!(message, "mic -> CABLE Input");
        }
        other => panic!("expected a HelloAck, got {other:?}"),
    }
}

#[test]
fn a_wrong_token_is_rejected_without_leaking_the_right_one() {
    let (host, vm) = pair();
    host.send(
        &Packet::Hello {
            token: b"guess".to_vec(),
            session: 1,
            mic: mic_format(),
            speaker: speaker_format(),
        }
        .encode(),
    )
    .unwrap();

    let mut buf = vec![0u8; proto::MAX_PACKET];
    let (n, peer) = vm.recv_from(&mut buf).unwrap();
    let Incoming::Control(Packet::Hello {
        token,
        session,
        mic,
        speaker,
    }) = proto::decode(&buf[..n]).unwrap()
    else {
        panic!("expected a Hello");
    };
    assert!(!proto::token_matches(TOKEN, &token));

    let ack = Packet::HelloAck {
        status: Status::AuthFailed,
        session,
        mic,
        speaker,
        message: String::new(),
    };
    vm.send_to(&ack.encode(), peer).unwrap();

    let n = host.recv(&mut buf).unwrap();
    match proto::decode(&buf[..n]).unwrap() {
        Incoming::Control(Packet::HelloAck {
            status, message, ..
        }) => {
            assert_eq!(status, Status::AuthFailed);
            assert!(
                message.is_empty(),
                "the reply must not describe the expected token"
            );
        }
        other => panic!("expected a HelloAck, got {other:?}"),
    }
}

/// A 10 ms stereo packet at 48 kHz is 1920 bytes of payload, which is over the
/// usual 1500-byte MTU. It has to survive the trip intact anyway.
#[test]
fn full_size_stereo_packets_survive_the_wire() {
    let (host, vm) = pair();
    let format = speaker_format();
    assert!(
        !format.fits_ethernet_mtu(),
        "this test is only interesting when it fragments"
    );

    let samples: Vec<i16> = (0..format.samples_per_packet())
        .map(|i| (i as i32 * 31 - 16384) as i16)
        .collect();
    let mut wire = Vec::new();
    proto::encode_audio(
        &mut wire,
        42,
        Direction::Speaker,
        7,
        format.channels,
        &samples,
    );
    vm.send_to(&wire, host.local_addr().unwrap()).unwrap();

    let mut buf = vec![0u8; proto::MAX_PACKET];
    let n = host.recv(&mut buf).unwrap();
    match proto::decode(&buf[..n]).unwrap() {
        Incoming::Audio { header, payload } => {
            assert_eq!(header.session, 42);
            assert_eq!(header.direction, Direction::Speaker);
            assert_eq!(header.seq, 7);
            assert_eq!(header.frames as usize, format.frames_per_packet());
            let mut decoded = Vec::new();
            proto::decode_samples(payload, &mut decoded);
            assert_eq!(decoded, samples);
        }
        other => panic!("expected audio, got {other:?}"),
    }
}

/// Drive a stream through encode -> a hostile network -> decode -> jitter
/// buffer, and confirm the receiver keeps producing continuous audio.
#[test]
fn audio_survives_loss_reordering_and_duplication() {
    let format = mic_format();
    let frames = format.frames_per_packet();
    let mut jitter = JitterBuffer::new(format.channels as usize, format.rate, format.frame_ms, 40);

    // A slow ramp that runs continuously across packet boundaries, so a glitch
    // would show up as a jump in the output.
    let sample_at = |n: usize| -> i16 { ((n % 2000) as i32 - 1000) as i16 * 16 };

    let mut wire = Vec::new();
    let mut decoded = Vec::new();
    let mut out = vec![0i16; frames];
    let mut produced: Vec<i16> = Vec::new();

    let deliver =
        |seq: u32, jitter: &mut JitterBuffer, wire: &mut Vec<u8>, decoded: &mut Vec<i16>| {
            let base = seq as usize * frames;
            let pcm: Vec<i16> = (0..frames).map(|i| sample_at(base + i)).collect();
            proto::encode_audio(wire, 1, Direction::Mic, seq, format.channels, &pcm);
            match proto::decode(wire).unwrap() {
                Incoming::Audio { header, payload } => {
                    proto::decode_samples(payload, decoded);
                    jitter.push(header.seq, decoded);
                }
                other => panic!("expected audio, got {other:?}"),
            }
        };

    // Prefill so the first pull is real audio rather than warm-up silence.
    for seq in 0..8 {
        deliver(seq, &mut jitter, &mut wire, &mut decoded);
    }

    for seq in 8..300u32 {
        match seq {
            50 => continue, // dropped in flight
            100 => {
                // 100 and 101 swap places
                deliver(101, &mut jitter, &mut wire, &mut decoded);
                deliver(100, &mut jitter, &mut wire, &mut decoded);
                continue;
            }
            101 => continue, // already delivered above
            150 => {
                // delivered twice
                deliver(150, &mut jitter, &mut wire, &mut decoded);
                deliver(150, &mut jitter, &mut wire, &mut decoded);
                continue;
            }
            _ => deliver(seq, &mut jitter, &mut wire, &mut decoded),
        }
        jitter.pull(&mut out);
        produced.extend_from_slice(&out);
    }

    let stats = jitter.stats();
    // Two "losses": the packet that really vanished, plus packet 100, which the
    // buffer had already given up on by the time it turned up behind 101. The
    // buffer deliberately does not reorder — on a LAN the extra latency that
    // would cost is a worse trade than concealing a rare swap.
    assert_eq!(stats.lost, 2);
    assert_eq!(
        stats.late, 2,
        "the duplicate and the late-arriving 100 are both discarded"
    );
    assert_eq!(stats.underruns, 0, "the buffer never ran dry");
    assert_eq!(stats.overruns, 0, "and never had to throw audio away");
    assert_eq!(stats.resyncs, 0);

    // The concealed gap fades, but nothing anywhere should exceed the source's
    // amplitude or sit at digital silence for a whole packet.
    assert!(produced.iter().all(|&s| s.unsigned_abs() <= 16_000));
    let silent_run = produced
        .chunks(frames)
        .filter(|c| c.iter().all(|&s| s == 0))
        .count();
    assert_eq!(silent_run, 0, "no packet-length dropout is allowed");
}

/// One lost packet must not desynchronise everything after it: the ramp has to
/// line up again once the gap has been concealed.
#[test]
fn the_stream_realigns_after_a_gap() {
    let format = mic_format();
    let frames = format.frames_per_packet();
    let mut jitter = JitterBuffer::new(format.channels as usize, format.rate, format.frame_ms, 40);

    let mut decoded = Vec::new();
    let mut wire = Vec::new();
    let value_of = |seq: u32| -> i16 { (seq as i16).wrapping_mul(101) };

    for seq in 0..12u32 {
        if seq == 6 {
            continue; // lost
        }
        let pcm = vec![value_of(seq); frames];
        proto::encode_audio(&mut wire, 1, Direction::Mic, seq, format.channels, &pcm);
        let Incoming::Audio { header, payload } = proto::decode(&wire).unwrap() else {
            panic!("expected audio");
        };
        proto::decode_samples(payload, &mut decoded);
        jitter.push(header.seq, &decoded);
    }

    // 12 packets were sent, 11 arrived, one gap was concealed: 12 slots total.
    assert_eq!(jitter.fill_frames(), frames * 12);
    assert_eq!(jitter.stats().lost, 1);

    let mut out = vec![0i16; frames * 12];
    jitter.pull(&mut out);

    // Packet 7's audio must land in packet 7's slot, not packet 6's.
    let slot7 = &out[frames * 7 + 8..frames * 8 - 8];
    assert!(
        slot7.iter().all(|&s| s == value_of(7)),
        "packet 7 did not land where it belongs"
    );
}
