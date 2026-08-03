//! Two things at scale: arbitrary bytes must not kill either decoder, and a
//! hole in a *valid* stream must not cost more than the packet it landed in.
//!
//! The arbitrary-byte runs are liveness only, and say so. Every invariant the
//! first version of them asserted was structurally guaranteed by code the run
//! never exercised — `usage != 0` by `emit`, the HID range by the tables (and
//! checked exhaustively in `decode.rs`), `buttons < 8` by `head & 0x07`,
//! `|dx| <= 256` by `delta`'s domain — so the assertions could not fail, and
//! nothing observed framing, which is the one property these decoders carry
//! real risk in.

use toyos_ps2::{KeyDecoder, KeyOutcome, MouseDecoder, MouseOutcome};

/// xorshift64*, so the run is reproducible and the generator is not the thing
/// under test.
struct Rng(u64);

impl Rng {
    fn next_byte(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 33) as u8
    }

    fn below(&mut self, n: u32) -> u32 {
        u32::from_le_bytes([
            self.next_byte(),
            self.next_byte(),
            self.next_byte(),
            self.next_byte(),
        ]) % n
    }
}

const BYTES: usize = 10_000_000;

/// A PS/2 byte is 11 bits at 10–16.7 kHz and the driver programs 100
/// samples/s, so this is the wire: three bytes about a millisecond apart, then
/// an idle trough until the next sample.
const BYTE_NS: u64 = 1_000_000;
const SAMPLE_NS: u64 = 10_000_000;

#[test]
fn arbitrary_bytes_never_kill_the_key_decoder() {
    let mut rng = Rng(0x2545F4914F6CDD1D);
    let mut d = KeyDecoder::new();
    let mut emitted = 0u64;
    let mut lost = 0u64;
    for i in 0..BYTES {
        // Occasionally do what a ring overflow does, since that is a state
        // transition the byte stream alone cannot reach.
        if i % 4096 == 0 {
            d.reset();
        }
        match d.feed(rng.next_byte()) {
            KeyOutcome::Key { .. } => emitted += 1,
            KeyOutcome::Lost => lost += 1,
            KeyOutcome::Pending | KeyOutcome::None => {}
        }
    }
    // The only two things a random stream can prove: it did not panic, and it
    // did not wedge in a prefix state, which would make every run silent.
    assert!(emitted > BYTES as u64 / 100, "only {emitted} usages from {BYTES} bytes");
    assert!(lost > 0, "never saw an overrun code in {BYTES} random bytes");
}

#[test]
fn arbitrary_bytes_never_kill_the_mouse_decoder() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut d = MouseDecoder::new();
    let mut now = 0u64;
    let mut packets = 0u64;
    let mut resets = 0u64;
    for i in 0..BYTES {
        if i % 4096 == 0 {
            d.reset();
        }
        // Bytes at wire pace, with the occasional idle trough — that gap is
        // the resync mechanism, so a run without one exercises the wrong path.
        now += if i % 977 == 0 { 200_000_000 } else { 100_000 };
        match d.feed(rng.next_byte(), now) {
            MouseOutcome::Packet { .. } => packets += 1,
            MouseOutcome::Reset => resets += 1,
            MouseOutcome::Pending | MouseOutcome::Discarded => {}
        }
    }
    assert!(packets > BYTES as u64 / 100, "only {packets} packets from {BYTES} bytes");
    assert!(resets > 0, "never saw a reset announcement in {BYTES} random bytes");
}

/// One packet on the wire, and what the decoder owes for it. No overflow bits:
/// the motion field is meaningless when they are set, and this is about
/// framing.
fn wire(buttons: u8, raw_x: i32, raw_y: i32) -> ([u8; 3], (u8, i32, i32)) {
    let mut head = 0x08 | (buttons & 0x07);
    if raw_x < 0 {
        head |= 0x10;
    }
    if raw_y < 0 {
        head |= 0x20;
    }
    ([head, raw_x as u8, raw_y as u8], (buttons & 0x07, raw_x, -raw_y))
}

/// The property the wire format itself cannot give: a lost byte costs the
/// packet it landed in and nothing after it.
///
/// This is what the always-one bit cannot do. Bit 3 is set in every legal head
/// byte *and* in body bytes like `dx = 10`, so a stream misaligned by one byte
/// completes a bogus group every time and stays misaligned; only the idle gap
/// between packets re-frames it. Deleting a byte from a million packets and
/// requiring every intact one to decode exactly is the test that says so.
#[test]
fn a_lost_byte_costs_exactly_the_packet_it_landed_in() {
    const PACKETS: usize = 1_000_000;
    let mut rng = Rng(0xD1B54A32D192ED03);
    let mut d = MouseDecoder::new();
    let mut holes = 0u64;
    let mut decoded = 0u64;
    let mut discarded = 0u64;
    for p in 0..PACKETS {
        let buttons = rng.next_byte() & 0x07;
        let raw_x = rng.below(512) as i32 - 256;
        let raw_y = rng.below(512) as i32 - 256;
        let (bytes, want) = wire(buttons, raw_x, raw_y);
        let hole = if rng.below(64) == 0 {
            holes += 1;
            Some(rng.below(3) as usize)
        } else {
            None
        };

        let mut got = None;
        for (i, &byte) in bytes.iter().enumerate() {
            if hole == Some(i) {
                continue;
            }
            let at = p as u64 * SAMPLE_NS + i as u64 * BYTE_NS;
            match d.feed(byte, at) {
                MouseOutcome::Packet { buttons, dx, dy } => {
                    assert!(got.is_none(), "packet {p} produced two packets");
                    got = Some((buttons, dx, dy));
                }
                // A misframed head of 0xAA followed by 0x00 is the one thing
                // that can invent a device reset — and one costs a masked line
                // and a polled handshake — so it must never happen outside the
                // packet a byte was taken from.
                MouseOutcome::Reset => assert!(
                    hole.is_some(),
                    "packet {p} reported a device reset out of an intact stream"
                ),
                MouseOutcome::Discarded => discarded += 1,
                MouseOutcome::Pending => {}
            }
        }
        match hole {
            None => {
                assert_eq!(got, Some(want), "packet {p} decoded wrong with no byte lost");
                decoded += 1;
            }
            Some(_) => assert_eq!(got, None, "packet {p} decoded a packet out of two bytes"),
        }
    }
    assert!(holes > 1000, "only {holes} holes in {PACKETS} packets");
    assert_eq!(decoded, PACKETS as u64 - holes, "an intact packet went missing");
    // The desync is bounded, and by a number: a hole leaves at most the two
    // remaining bytes of its own packet at a boundary they cannot be a head
    // of, and the idle gap re-frames the next one. A driver that counts
    // discards to decide whether its pointer has lost the frame is reading
    // this bound, so an intact stream costing even one of them would make the
    // count unreadable.
    assert!(
        discarded <= 2 * holes,
        "{discarded} bytes discarded for {holes} holes — the desync outran the packet"
    );
}

/// The longest prefix in set 1 is Pause (`E1 1D 45 E1 9D C5`), so a byte lost
/// anywhere can cost at most the six bytes that sequence spans. Past that the
/// decoder must agree, byte for byte, with one that never saw the hole.
#[test]
fn a_lost_scancode_byte_never_wedges_the_prefix_state() {
    const MAX_PREFIX: usize = 6;
    // Plain, E0-prefixed and Pause are the three shapes the state machine has;
    // mixing them is what puts a hole in each.
    const SEQUENCES: [&[u8]; 4] = [
        &[0x1E, 0x9E],                         // 'a' make/break
        &[0xE0, 0x4B, 0xE0, 0xCB],             // left arrow make/break
        &[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5], // Pause
        &[0x2A, 0x1E, 0x9E, 0xAA],             // Shift+'a'
    ];
    let marker: Vec<u8> = SEQUENCES[0].repeat(8);
    let mut fresh = KeyDecoder::new();
    let want: Vec<KeyOutcome> = marker.iter().map(|&b| fresh.feed(b)).collect();

    let mut rng = Rng(0x14057B7EF767814F);
    for trial in 0..20_000 {
        let mut stream = Vec::new();
        for _ in 0..8 {
            stream.extend_from_slice(SEQUENCES[rng.below(4) as usize]);
        }
        stream.remove(rng.below(stream.len() as u32) as usize);

        let mut d = KeyDecoder::new();
        for &b in &stream {
            d.feed(b);
        }
        // The whole marker goes in — a decoder still swallowing a prefix has
        // to be *given* the bytes it swallows — and only what it says past
        // the sequence-length bound is compared.
        let got: Vec<KeyOutcome> = marker.iter().map(|&b| d.feed(b)).collect();
        assert_eq!(
            got[MAX_PREFIX..],
            want[MAX_PREFIX..],
            "trial {trial}: a lost byte left the decoder out of step past {MAX_PREFIX} bytes"
        );
    }
}
