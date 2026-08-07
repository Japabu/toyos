//! Stream format, the cyclic buffer descriptor list, and which periods played.
//!
//! Boundary contract: `SDnFMT`'s field layout and the descriptor's shape come
//! from the Intel High Definition Audio specification. `specs/hda-driver-plan.md`
//! §2.4 is the design — one descriptor per period, no submit in steady state,
//! and the completion mask derived from a position read rather than from
//! counting interrupts.

use alloc::vec::Vec;

/// The two rate bases the format field can name, and the only two.
const BASES: [u32; 2] = [48_000, 44_100];

/// How many periods a cyclic buffer may have.
///
/// Policy: the mask is a `u32` and soundd's pipeline is eight deep. A caller
/// past this is told, rather than handed a mask with bits missing.
pub const MAX_PERIODS: usize = 32;

/// A descriptor list has at least two entries — a one-entry ring gives the
/// engine nothing to wrap to and the specification forbids it.
pub const MIN_PERIODS: usize = 2;

/// `SDnFMT` for one PCM stream, or `None` when the format cannot be expressed.
///
/// The rate is searched over the base, multiplier and divisor the field
/// actually has rather than looked up in a table of common values: an exact
/// match is the only correct answer, and a rate this cannot express is a
/// refusal the caller reports rather than a nearby rate it substitutes.
pub fn stream_format(rate: u32, bits: u8, channels: u8) -> Option<u16> {
    let width = match bits {
        8 => 0b000,
        16 => 0b001,
        20 => 0b010,
        24 => 0b011,
        32 => 0b100,
        _ => return None,
    };
    if channels == 0 || channels > 16 {
        return None;
    }

    for (base_bit, base) in BASES.iter().enumerate() {
        for mult in 1..=4u32 {
            for div in 1..=8u32 {
                if base * mult % div != 0 || base * mult / div != rate {
                    continue;
                }
                return Some(
                    ((base_bit as u16) << 13)
                        | (((mult - 1) as u16) << 11)
                        | (((div - 1) as u16) << 8)
                        | (width << 4)
                        | (channels - 1) as u16,
                );
            }
        }
    }
    None
}

/// One buffer descriptor: where a period's samples are and whether the engine
/// raises an interrupt when it has played them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BdlEntry {
    pub address: u64,
    pub length: u32,
    pub interrupt_on_completion: bool,
}

/// The cyclic descriptor list for a buffer of `periods` equal periods.
///
/// `None` for a shape the engine cannot run: fewer than two periods, more than
/// this crate will mask, a zero-length period, or a buffer whose end would
/// wrap past the address space.
pub fn build_bdl(base: u64, period_bytes: u32, periods: usize) -> Option<Vec<BdlEntry>> {
    if !(MIN_PERIODS..=MAX_PERIODS).contains(&periods) || period_bytes == 0 {
        return None;
    }
    let total = (period_bytes as u64).checked_mul(periods as u64)?;
    base.checked_add(total)?;
    Some(
        (0..periods)
            .map(|i| BdlEntry {
                address: base + i as u64 * period_bytes as u64,
                length: period_bytes,
                interrupt_on_completion: true,
            })
            .collect(),
    )
}

/// The whole cyclic length, which is what `SDnCBL` is set to.
pub fn cyclic_length(period_bytes: u32, periods: usize) -> Option<u32> {
    (period_bytes as u64 * periods as u64).try_into().ok()
}

/// `SDnLVI`: the index of the last valid descriptor, set once.
pub fn last_valid_index(periods: usize) -> Option<u8> {
    (MIN_PERIODS..=MAX_PERIODS).contains(&periods).then(|| (periods - 1) as u8)
}

/// Which periods have played since `last`, and where the engine is now.
///
/// Derived from a position read and never from counting interrupts: one
/// interrupt can cover several periods, and soundd's mix loop already asserts
/// that a completion never repeats a buffer it still holds.
///
/// `position` is the device's own byte offset and is checked against the
/// buffer rather than trusted: a read past the end is a device answer this
/// cannot turn into an index, not a period to mark played.
///
/// A caller that sleeps through the whole ring sees the position where it left
/// it and is told nothing completed. That aliasing is why the buffer is zeroed
/// at completion (§2.4) — the engine replays silence rather than the last
/// period — and why the pipeline is deeper than any expected wake.
pub fn completed(
    last: usize,
    position: u32,
    period_bytes: u32,
    periods: usize,
) -> Option<(u32, usize)> {
    if !(MIN_PERIODS..=MAX_PERIODS).contains(&periods) || period_bytes == 0 || last >= periods {
        return None;
    }
    let total = period_bytes as u64 * periods as u64;
    if position as u64 >= total {
        return None;
    }
    let current = (position / period_bytes) as usize;
    let mut mask = 0u32;
    let mut index = last;
    while index != current {
        mask |= 1 << index;
        index = (index + 1) % periods;
    }
    Some((mask, current))
}

/// What a driver's accumulated completion mask says about the engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Completed {
    /// The engine handed back `count` periods starting at `first`, and is in
    /// `first + count` now.
    Run { first: usize, count: usize },
    /// Every period in the ring. The engine has been round at least once since
    /// the driver last looked, so nothing in the mask says where it is — the
    /// aliasing [`completed`] documents, arriving one level up, where it means
    /// the whole pipeline has played out.
    Lapped,
}

/// Read an accumulated completion mask.
///
/// A mask is a set and the engine is a sequence, so the set alone has to say
/// which period the engine returns to *first* — and that is the mask's
/// lowest-numbered bit only while the run does not wrap the ring. `{6, 7, 0,
/// 1}` is played 6, 7, 0, 1, and a driver filling it lowest-index-first writes
/// the later audio into the buffer the engine reaches soonest.
///
/// It is always one contiguous run, because a driver reading late sees the OR
/// of consecutive [`completed`] calls and those abut. `None` for anything else:
/// no sequence of them can produce it, so it is a bug on the side that built it
/// rather than a position to act on.
pub fn decode(mask: u32, periods: usize) -> Option<Completed> {
    if !(MIN_PERIODS..=MAX_PERIODS).contains(&periods) || mask >> periods != 0 {
        return None;
    }
    let count = mask.count_ones() as usize;
    if count == periods {
        return Some(Completed::Lapped);
    }
    let first = (0..periods)
        .find(|&i| mask & 1 << i != 0 && mask & 1 << ((i + periods - 1) % periods) == 0)?;
    ((0..count).fold(0u32, |run, i| run | 1 << ((first + i) % periods)) == mask)
        .then_some(Completed::Run { first, count })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_this_pipeline_plays_encodes_as_the_forty_four_one_base() {
        // 44.1 kHz, S16, stereo — soundd's grid, and what both the T14's
        // converter and QEMU's offer.
        assert_eq!(stream_format(44_100, 16, 2), Some(0x2011));
    }

    #[test]
    fn forty_eight_kilohertz_is_the_other_base() {
        assert_eq!(stream_format(48_000, 16, 2), Some(0x0011));
    }

    #[test]
    fn a_divided_rate_uses_the_divisor_field() {
        // 22.05 kHz is 44.1 halved, not a base of its own.
        assert_eq!(stream_format(22_050, 16, 2), Some(0x2111));
        // 24 kHz is 48 halved.
        assert_eq!(stream_format(24_000, 16, 2), Some(0x0111));
    }

    #[test]
    fn a_multiplied_rate_uses_the_multiplier_field() {
        assert_eq!(stream_format(96_000, 16, 2), Some(0x0811));
        assert_eq!(stream_format(88_200, 16, 2), Some(0x2811));
    }

    #[test]
    fn widths_and_channel_counts_land_in_their_own_fields() {
        assert_eq!(stream_format(48_000, 24, 2), Some(0x0031));
        assert_eq!(stream_format(48_000, 16, 8), Some(0x0017));
    }

    #[test]
    fn a_rate_the_field_cannot_express_is_refused_and_not_rounded() {
        assert_eq!(stream_format(44_000, 16, 2), None);
        assert_eq!(stream_format(0, 16, 2), None);
    }

    #[test]
    fn an_unrepresentable_width_or_channel_count_is_refused() {
        assert_eq!(stream_format(48_000, 12, 2), None);
        assert_eq!(stream_format(48_000, 16, 0), None);
        assert_eq!(stream_format(48_000, 16, 17), None);
    }

    #[test]
    fn the_pipeline_s_own_shape_builds_a_ring() {
        // soundd: eight buffers of 512 bytes.
        let bdl = build_bdl(0x1000, 512, 8).unwrap();
        assert_eq!(bdl.len(), 8);
        assert_eq!(bdl[0].address, 0x1000);
        assert_eq!(bdl[7].address, 0x1000 + 7 * 512);
        assert!(bdl.iter().all(|e| e.length == 512 && e.interrupt_on_completion));
        assert_eq!(cyclic_length(512, 8), Some(4096));
        assert_eq!(last_valid_index(8), Some(7));
    }

    #[test]
    fn a_ring_with_nothing_to_wrap_to_is_refused() {
        assert_eq!(build_bdl(0x1000, 512, 1), None);
        assert_eq!(build_bdl(0x1000, 512, 0), None);
        assert_eq!(last_valid_index(1), None);
    }

    #[test]
    fn a_period_count_past_the_mask_is_refused_rather_than_masked_short() {
        assert_eq!(build_bdl(0x1000, 512, MAX_PERIODS + 1), None);
        assert!(build_bdl(0x1000, 512, MAX_PERIODS).is_some());
    }

    #[test]
    fn a_buffer_that_would_wrap_the_address_space_is_refused() {
        assert_eq!(build_bdl(u64::MAX - 1024, 512, 8), None);
    }

    #[test]
    fn completion_marks_every_period_between_two_positions() {
        // The engine is playing period 3; periods 0, 1 and 2 have played.
        assert_eq!(completed(0, 3 * 512, 512, 8), Some((0b0000_0111, 3)));
    }

    #[test]
    fn completion_wraps_the_ring() {
        // Was at 6, now playing 1: 6, 7 and 0 have played.
        assert_eq!(completed(6, 512, 512, 8), Some((0b1100_0001, 1)));
    }

    #[test]
    fn a_position_inside_the_period_still_being_played_marks_nothing_new() {
        assert_eq!(completed(3, 3 * 512 + 200, 512, 8), Some((0, 3)));
    }

    #[test]
    fn a_position_past_the_buffer_is_not_an_index() {
        // The device's own number, and one that cannot be turned into a
        // period: masking it to fit would mark a buffer played that was not.
        assert_eq!(completed(0, 4096, 512, 8), None);
        assert_eq!(completed(0, u32::MAX, 512, 8), None);
    }

    #[test]
    fn a_last_index_outside_the_ring_is_refused() {
        assert_eq!(completed(8, 0, 512, 8), None);
    }

    #[test]
    fn a_mask_names_the_period_the_engine_returns_to_first() {
        assert_eq!(decode(0b0011_1000, 8), Some(Completed::Run { first: 3, count: 3 }));
        assert_eq!(decode(0b0000_0001, 8), Some(Completed::Run { first: 0, count: 1 }));
    }

    #[test]
    fn a_run_that_wraps_does_not_start_at_its_lowest_bit() {
        // The engine was at 6 and is now playing 2: 6, 7, 0 and 1 have played
        // and are played again in that order. A driver reading the mask
        // lowest-bit-first fills 0, 1, 6, 7 — the later audio into the two
        // buffers the engine reaches soonest, which is a splice with no silence
        // in it for a gap detector to see.
        let (mask, _) = completed(6, 2 * 512, 512, 8).unwrap();
        assert_eq!(decode(mask, 8), Some(Completed::Run { first: 6, count: 4 }));
        assert_ne!(mask.trailing_zeros(), 6);
    }

    #[test]
    fn every_position_completed_can_report_reads_back_as_the_run_it_walked() {
        for last in 0..8usize {
            for position in 0..8 * 512u32 {
                let (mask, _) = completed(last, position, 512, 8).unwrap();
                let count = mask.count_ones() as usize;
                let want = (count > 0).then_some(Completed::Run { first: last, count });
                assert_eq!(decode(mask, 8), want.or(decode(0, 8)), "last={last} pos={position}");
            }
        }
    }

    #[test]
    fn a_driver_that_slept_a_whole_lap_is_told_the_mask_cannot_place_the_engine() {
        // What a reader sees is the OR of every `completed` since it last
        // looked, so a full ring is reachable where one call's is not.
        let mut mask = 0;
        for last in 0..8usize {
            mask |= completed(last, ((last as u32 + 1) % 8) * 512, 512, 8).unwrap().0;
        }
        assert_eq!(mask, 0xFF);
        assert_eq!(decode(mask, 8), Some(Completed::Lapped));
    }

    #[test]
    fn a_mask_that_is_not_one_run_is_refused_rather_than_placed() {
        // Two disjoint runs: no walk of the ring produces it, so it is a bug on
        // the side that built the mask and not a position to fill from.
        assert_eq!(decode(0b0010_0101, 8), None);
        // A bit outside the ring.
        assert_eq!(decode(1 << 8, 8), None);
        assert_eq!(decode(1, 1), None);
    }
}
