//! Ten million arbitrary bytes through both decoders.
//!
//! A PS/2 line carries whatever the EC feels like sending, including the
//! middle of a sequence we never saw the start of. The invariants below are
//! the ones the rest of the kernel relies on without checking: a usage of 0
//! would be queued as a keypress nothing can name, and a delta outside 9 bits
//! would mean the sign extension is wrong somewhere.

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
}

const BYTES: usize = 10_000_000;

#[test]
fn keys_never_emit_an_unnameable_usage() {
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
            KeyOutcome::Key { usage, .. } => {
                assert!(usage != 0, "emitted the unmapped sentinel as a usage");
                assert!(
                    (0x04..=0x65).contains(&usage) || (0xE0..=0xE7).contains(&usage),
                    "emitted {usage:#04x}, which is not a HID keyboard usage"
                );
                emitted += 1;
            }
            KeyOutcome::Lost => lost += 1,
            KeyOutcome::None => {}
        }
    }
    // If the decoder had wedged in a prefix state it would emit nothing at
    // all, and every assertion above would hold vacuously.
    assert!(emitted > BYTES as u64 / 100, "only {emitted} usages from {BYTES} bytes");
    assert!(lost > 0, "never saw an overrun code in {BYTES} random bytes");
}

#[test]
fn mouse_deltas_stay_inside_nine_bits() {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut d = MouseDecoder::new();
    let mut now = 0u64;
    let mut packets = 0u64;
    let mut resets = 0u64;
    for i in 0..BYTES {
        if i % 4096 == 0 {
            d.reset();
        }
        // Bytes arrive at wire pace; every so often a gap long enough to
        // strand a partial packet, which is its own code path.
        now += if i % 977 == 0 { 200_000_000 } else { 100_000 };
        match d.feed(rng.next_byte(), now) {
            MouseOutcome::Packet { buttons, dx, dy } => {
                assert!(buttons < 8, "buttons {buttons:#04x} has bits outside the low three");
                assert!(dx.abs() <= 256, "dx {dx} is outside a 9-bit delta");
                assert!(dy.abs() <= 256, "dy {dy} is outside a 9-bit delta");
                packets += 1;
            }
            MouseOutcome::Reset => resets += 1,
            MouseOutcome::None => {}
        }
    }
    assert!(packets > BYTES as u64 / 100, "only {packets} packets from {BYTES} bytes");
    assert!(resets > 0, "never saw a reset announcement in {BYTES} random bytes");
}
