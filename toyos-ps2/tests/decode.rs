//! What the decoders promise, checked against the wire formats they claim to
//! speak. Nothing here needs QEMU, and none of it can be single-stepped on
//! the machine it exists for.

use toyos_ps2::key::{SET1, SET1_E0};
use toyos_ps2::{KeyDecoder, KeyOutcome, MouseDecoder, MouseOutcome};

/// Every HID keyboard usage the layouts and the compositor can name. Outside
/// these two ranges a usage is not a keyboard usage at all.
fn is_hid_keyboard_usage(usage: u8) -> bool {
    (0x04..=0x65).contains(&usage) || (0xE0..=0xE7).contains(&usage)
}

fn press(decoder: &mut KeyDecoder, bytes: &[u8]) -> Vec<(u8, bool)> {
    let mut out = Vec::new();
    for &b in bytes {
        if let KeyOutcome::Key { usage, pressed } = decoder.feed(b) {
            out.push((usage, pressed));
        }
    }
    out
}

#[test]
fn tables_hold_only_real_usages_and_no_duplicates() {
    for (name, table) in [("SET1", &SET1), ("SET1_E0", &SET1_E0)] {
        let mut seen = [false; 256];
        for (code, &usage) in table.iter().enumerate() {
            if usage == 0 {
                continue;
            }
            assert!(
                is_hid_keyboard_usage(usage),
                "{name}[{code:#04x}] = {usage:#04x} is not a HID keyboard usage"
            );
            assert!(
                !seen[usage as usize],
                "{name} maps two scancodes to usage {usage:#04x}; {code:#04x} is the second"
            );
            seen[usage as usize] = true;
        }
    }
}

/// The usages `keyboard.rs` and the compositor actually key off. A table that
/// silently loses one of these is the difference between a working keyboard
/// and a mysterious one.
#[test]
fn every_usage_the_rest_of_the_tree_names_is_reachable() {
    let mut reachable = [false; 256];
    for &usage in SET1.iter().chain(SET1_E0.iter()) {
        reachable[usage as usize] = true;
    }
    // Letters, digits and punctuation (layout_lookup's 0x04..=0x38), the ISO
    // key, the escape-sequence keys, and every modifier.
    let required: Vec<u8> = (0x04u8..=0x38)
        .chain([0x64])
        .chain(0x4Au8..=0x52)
        .chain(0xE0u8..=0xE7)
        .collect();
    for usage in required {
        // 0x32 is the non-US hash/tilde position, which set 1 does not have a
        // distinct code for; it is `K` (blank) in every layout we ship.
        if usage == 0x32 {
            continue;
        }
        assert!(reachable[usage as usize], "no scancode produces HID usage {usage:#04x}");
    }
}

#[test]
fn the_iso_key_is_the_one_the_layouts_carry() {
    // Between left Shift and Y on the German/Swiss board a ThinkPad has.
    // Getting this wrong is the difference between `<>|` working and not.
    assert_eq!(SET1[0x56], 0x64);
}

#[test]
fn make_and_break_and_the_e0_prefix() {
    let mut d = KeyDecoder::new();
    assert_eq!(press(&mut d, &[0x1E]), [(0x04, true)]);
    assert_eq!(press(&mut d, &[0x9E]), [(0x04, false)]);
    // Left arrow is E0 4B / E0 CB.
    assert_eq!(press(&mut d, &[0xE0, 0x4B]), [(0x50, true)]);
    assert_eq!(press(&mut d, &[0xE0, 0xCB]), [(0x50, false)]);
    // The unprefixed 0x4B is keypad 4, a different key entirely.
    assert_eq!(press(&mut d, &[0x4B]), [(0x5C, true)]);
}

/// The panic console's pager reads these two straight off a halted controller
/// with its own decoder, and its whole input vocabulary is what this asserts.
#[test]
fn the_pager_keys_decode_to_their_hid_usages() {
    let mut d = KeyDecoder::new();
    assert_eq!(press(&mut d, &[0xE0, 0x49]), [(0x4B, true)], "PageUp make");
    assert_eq!(press(&mut d, &[0xE0, 0xC9]), [(0x4B, false)], "PageUp break");
    assert_eq!(press(&mut d, &[0xE0, 0x51]), [(0x4E, true)], "PageDown make");
    assert_eq!(press(&mut d, &[0xE0, 0xD1]), [(0x4E, false)], "PageDown break");
    // Unprefixed they are the keypad's 9 and 3, which a pager must not answer.
    assert_eq!(press(&mut d, &[0x49]), [(0x61, true)]);
    assert_eq!(press(&mut d, &[0x51]), [(0x5B, true)]);
}

#[test]
fn printscreen_emits_one_usage_and_no_phantom_shift() {
    let mut d = KeyDecoder::new();
    // Under translation PrtScn is E0 2A E0 37 make, E0 B7 E0 AA break.
    assert_eq!(press(&mut d, &[0xE0, 0x2A, 0xE0, 0x37]), [(0x46, true)]);
    assert_eq!(press(&mut d, &[0xE0, 0xB7, 0xE0, 0xAA]), [(0x46, false)]);
}

#[test]
fn pause_consumes_exactly_six_bytes_and_emits_nothing() {
    let pause = [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5];

    let mut d = KeyDecoder::new();
    assert_eq!(press(&mut d, &pause), []);
    // And the stream is still framed: the next byte is decoded normally.
    assert_eq!(press(&mut d, &[0x1E]), [(0x04, true)]);

    // Mid-stream, between two ordinary keys.
    let mut d = KeyDecoder::new();
    let mut stream = vec![0x1E];
    stream.extend_from_slice(&pause);
    stream.push(0x9E);
    assert_eq!(press(&mut d, &stream), [(0x04, true), (0x04, false)]);
}

#[test]
fn left_shift_break_is_not_read_as_a_reset() {
    // 0xAA is both the keyboard's BAT-complete byte and left Shift's break
    // code under translation. They are indistinguishable, so it must decode
    // as the key — silently dropping every Shift release would be worse.
    let mut d = KeyDecoder::new();
    assert_eq!(press(&mut d, &[0x2A]), [(0xE1, true)]);
    assert_eq!(press(&mut d, &[0xAA]), [(0xE1, false)]);
}

#[test]
fn overrun_codes_report_lost_and_clear_the_prefix_state() {
    let mut d = KeyDecoder::new();
    assert_eq!(d.feed(0x00), KeyOutcome::Lost);
    assert_eq!(d.feed(0xFF), KeyOutcome::Lost);

    // A prefix whose second byte was lost must not mis-decode the next one.
    let mut d = KeyDecoder::new();
    assert_eq!(d.feed(0xE0), KeyOutcome::Pending);
    d.reset();
    assert_eq!(press(&mut d, &[0x4B]), [(0x5C, true)], "reset left the E0 prefix pending");
}

/// A byte with more of its sequence to come is not a byte that produced
/// nothing, and a driver reporting the second must not report the first.
/// `0xE0` leads every arrow key on a working keyboard; listing it beside a
/// genuinely undecodable byte is what makes such a list unreadable.
#[test]
fn a_sequence_is_pending_until_the_byte_that_ends_it() {
    let mut d = KeyDecoder::new();
    // Left arrow: the prefix is pending, the second byte names the key.
    assert_eq!(d.feed(0xE0), KeyOutcome::Pending);
    assert_eq!(d.feed(0x4B), KeyOutcome::Key { usage: 0x50, pressed: true });

    // Pause names nothing at all, and it is the *last* of its six bytes that
    // says so — the five before it are still a sequence in progress.
    let mut d = KeyDecoder::new();
    let pause = [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5];
    let outcomes: Vec<KeyOutcome> = pause.iter().map(|&b| d.feed(b)).collect();
    assert_eq!(
        outcomes,
        [
            KeyOutcome::Pending,
            KeyOutcome::Pending,
            KeyOutcome::Pending,
            KeyOutcome::Pending,
            KeyOutcome::Pending,
            KeyOutcome::None,
        ]
    );

    // An extended code ToyOS has no usage for ends its sequence the same way.
    let mut d = KeyDecoder::new();
    assert_eq!(d.feed(0xE0), KeyOutcome::Pending);
    assert_eq!(d.feed(0x18), KeyOutcome::None, "SET1_E0[0x18] is unmapped");
}

#[test]
fn unmapped_scancodes_emit_nothing() {
    // 0x00/0xFF are the overrun codes and 0xE0/0xE1 the prefixes; each has
    // its own test above and none of them is a key.
    let reserved = |b: u8| matches!(b, 0x00 | 0xFF | 0xE0 | 0xE1);
    let mut d = KeyDecoder::new();
    for code in 0u8..=0x7F {
        if SET1[code as usize] != 0 {
            continue;
        }
        if !reserved(code) {
            assert_eq!(d.feed(code), KeyOutcome::None, "scancode {code:#04x} is unmapped");
        }
        if !reserved(code | 0x80) {
            assert_eq!(d.feed(code | 0x80), KeyOutcome::None);
        }
    }
}

// Mouse

fn packet(buttons: u8, dx: i16, dy: i16) -> [u8; 3] {
    let mut head = 0x08 | (buttons & 0x07);
    if dx < 0 {
        head |= 0x10;
    }
    if dy < 0 {
        head |= 0x20;
    }
    [head, dx as u8, dy as u8]
}

fn feed_all(d: &mut MouseDecoder, bytes: &[u8]) -> Vec<MouseOutcome> {
    bytes
        .iter()
        .map(|&b| d.feed(b, 0))
        .filter(|o| !matches!(o, MouseOutcome::Pending | MouseOutcome::Discarded))
        .collect()
}

#[test]
fn deltas_are_nine_bit_and_dy_is_inverted() {
    let mut d = MouseDecoder::new();
    // The case `byte as i8` gets backwards: +200 has no overflow set.
    assert_eq!(
        feed_all(&mut d, &packet(0, 200, 0)),
        [MouseOutcome::Packet { buttons: 0, dx: 200, dy: 0 }]
    );
    // Full 9-bit round trip, both axes, dy screen-oriented.
    for v in -256i16..=255 {
        let mut d = MouseDecoder::new();
        assert_eq!(
            feed_all(&mut d, &packet(0, v, v)),
            [MouseOutcome::Packet { buttons: 0, dx: v as i32, dy: -(v as i32) }],
            "delta {v} did not round trip"
        );
    }
    // PS/2 positive dy is up; the screen's is down.
    let mut d = MouseDecoder::new();
    assert_eq!(
        feed_all(&mut d, &packet(0, 0, 30)),
        [MouseOutcome::Packet { buttons: 0, dx: 0, dy: -30 }]
    );
}

#[test]
fn overflow_drops_the_motion_and_keeps_the_buttons() {
    let mut d = MouseDecoder::new();
    assert_eq!(
        feed_all(&mut d, &[0x08 | 0x01 | 0x40, 0x7F, 0x7F]),
        [MouseOutcome::Packet { buttons: 1, dx: 0, dy: 0 }]
    );
}

#[test]
fn buttons_are_already_in_hid_order() {
    for buttons in 0u8..8 {
        let mut d = MouseDecoder::new();
        assert_eq!(
            feed_all(&mut d, &packet(buttons, 1, 1)),
            [MouseOutcome::Packet { buttons, dx: 1, dy: -1 }]
        );
    }
}

/// A PS/2 byte is 11 bits at 10–16.7 kHz and the device is programmed for 100
/// samples/s, so a packet's bytes are ~1 ms apart and the next packet is a
/// sample period after this one started.
const BYTE_NS: u64 = 1_000_000;
const SAMPLE_NS: u64 = 10_000_000;

/// `count` packets as they arrive on the wire, minus the first `drop` bytes of
/// the first one. Each byte carries the time it would actually have arrived —
/// which is the whole resync mechanism, so a stream without it tests nothing.
fn timed_stream(good: [u8; 3], count: usize, drop: usize) -> Vec<(u8, u64)> {
    let mut out = Vec::new();
    for p in 0..count {
        for (i, &b) in good.iter().enumerate() {
            if p > 0 || i >= drop {
                out.push((b, p as u64 * SAMPLE_NS + i as u64 * BYTE_NS));
            }
        }
    }
    out
}

#[test]
fn a_stream_truncated_at_any_offset_resyncs_within_two_packets() {
    // `(10, 8)` is the case that matters: both body bytes have bit 3 set, so
    // both are legal head bytes and the always-one rule can discard neither —
    // a one-byte misframe then completes a bogus group every time and sustains
    // itself. `(5, 7)` is the opposite case, where bit 3 alone does resync.
    for (dx, dy) in [(10i16, 8i16), (5, 7)] {
        let good = packet(0, dx, dy);
        for drop in 0..3usize {
            let stream = timed_stream(good, 5, drop);
            let mut d = MouseDecoder::new();
            let mut packets = Vec::new();
            let mut consumed = 0;
            for (i, &(b, at)) in stream.iter().enumerate() {
                if let MouseOutcome::Packet { dx, dy, .. } = d.feed(b, at) {
                    if packets.is_empty() {
                        consumed = i + 1;
                    }
                    packets.push((dx, dy));
                }
            }
            assert!(
                !packets.is_empty(),
                "dropping {drop} leading byte(s) of ({dx}, {dy}) never resynced"
            );
            assert!(
                consumed <= 3 + 2 * 3,
                "dropping {drop} byte(s) of ({dx}, {dy}) took {consumed} bytes, more than two packets"
            );
            // And once resynced it stays correct, not merely aligned.
            for p in &packets {
                assert_eq!(
                    *p,
                    (dx as i32, -(dy as i32)),
                    "resynced to the wrong offset after dropping {drop} of ({dx}, {dy})"
                );
            }
        }
    }
}

#[test]
fn a_stale_partial_is_abandoned_rather_than_completed() {
    let mut d = MouseDecoder::new();
    assert_eq!(d.feed(0x08, 0), MouseOutcome::Pending);
    assert_eq!(d.feed(5, BYTE_NS), MouseOutcome::Pending);
    // The third byte arrives a second later — that is not one gesture. It is
    // read as a head instead, and `7` cannot be one.
    assert_eq!(d.feed(7, 1_000_000_000), MouseOutcome::Discarded);
    // The next whole packet decodes cleanly rather than one byte off.
    assert_eq!(
        feed_all(&mut d, &packet(0, 5, 7)),
        [MouseOutcome::Packet { buttons: 0, dx: 5, dy: -7 }]
    );
}

/// The distinction the ThinkPad T14's log could not make.
///
/// Its i8042 reported `6 bytes, 0 keys, 2 motion, no event from [aux 0x08,
/// aux 0x06, aux 0x08, aux 0x0e]` — two whole, correctly framed packets, whose
/// four non-final bytes the driver listed as suspects because the decoder
/// called them the same thing it calls a byte it threw away. A healthy stream
/// must discard nothing at all, and the bytes it consumes on the way to a
/// packet must not be confusable with the ones it rejects.
#[test]
fn a_healthy_stream_discards_nothing_and_leaves_no_byte_unaccounted() {
    let mut d = MouseDecoder::new();
    // The T14's own two packets, at the pace they arrived on its wire.
    for (p, dx) in [6i16, 14].into_iter().enumerate() {
        let good = packet(0, dx, 0);
        let mut outcomes = Vec::new();
        for (i, &b) in good.iter().enumerate() {
            outcomes.push(d.feed(b, p as u64 * SAMPLE_NS + i as u64 * BYTE_NS));
        }
        assert_eq!(
            outcomes,
            [
                MouseOutcome::Pending,
                MouseOutcome::Pending,
                MouseOutcome::Packet { buttons: 0, dx: dx as i32, dy: 0 },
            ],
            "packet {p} did not account for all three of its bytes"
        );
    }
}

/// `Discarded` is only ever the byte-level resync, and it is bounded by the
/// number of bytes that cannot be a head. One implausible byte at a boundary
/// costs one byte and nothing else — not the packet after it, and not the
/// framing.
#[test]
fn an_implausible_byte_at_a_boundary_costs_exactly_itself() {
    let mut d = MouseDecoder::new();
    // Bit 3 clear: no legal head byte looks like this.
    assert_eq!(d.feed(0x06, 0), MouseOutcome::Discarded);
    assert_eq!(d.feed(0x00, BYTE_NS), MouseOutcome::Discarded);
    assert_eq!(
        feed_all(&mut d, &packet(0, 5, 7)),
        [MouseOutcome::Packet { buttons: 0, dx: 5, dy: -7 }],
        "the packet after two discarded bytes did not decode"
    );
}

/// The other half: a byte the framer *does* accept as a head is never a
/// `Discarded`, however little sense the packet it starts turns out to make.
/// A caller counting discards to measure a desync must not be handed one for
/// every resting sample.
#[test]
fn no_legal_head_byte_is_ever_discarded() {
    for head in 0u8..=0xFF {
        let mut d = MouseDecoder::new();
        let first = d.feed(head, 0);
        if head & 0x08 == 0 {
            assert_eq!(first, MouseOutcome::Discarded, "{head:#04x} has bit 3 clear");
            continue;
        }
        assert_eq!(first, MouseOutcome::Pending, "{head:#04x} is a legal head byte");
        // And its body bytes are pending too, whatever they are — except
        // behind 0xAA, where `0x00` is the reset announcement and an event in
        // its own right.
        let body = if head == 0xAA { 0x01 } else { 0x00 };
        assert_eq!(d.feed(body, BYTE_NS), MouseOutcome::Pending);
    }
}

#[test]
fn a_device_reset_is_reported_and_aa_is_still_a_legal_head_byte() {
    let mut d = MouseDecoder::new();
    assert_eq!(feed_all(&mut d, &[0xAA, 0x00]), [MouseOutcome::Reset]);

    // 0xAA leads a packet as often as it announces a reset; both overflow
    // bits are set in it, so its motion is dropped and its buttons stand.
    let mut d = MouseDecoder::new();
    assert_eq!(
        feed_all(&mut d, &[0xAA, 0x11, 0x22]),
        [MouseOutcome::Packet { buttons: 0x02, dx: 0, dy: 0 }]
    );
}
