//! Everything soundd decides about a sample, as pure functions over pure state.
//!
//! What a client's `i16` becomes on the bus, what the sum of every client
//! becomes at the wire, what a channel count conversion does to it, what gain
//! rides on it and how that gain moves, plus the arithmetic those rest on: the
//! device shapes a period can be rendered into, the period sizes a client's
//! rate implies, the delay-locked loop that tracks the device's grid, and the
//! counters the audio gate reads. No devices, no handles, no shared memory, no
//! timers — those are `userland/soundd/`'s, and it is the only caller.
//!
//! **The split exists because a QEMU boot cannot ask any of these questions.**
//! Gate A certifies the device end — periods reaching the wire on time, a
//! stream not interrupted — and there is nothing in a guest that can say
//! whether the sample a client wrote is the sample the device played. That is
//! the half a listener hears, and `corpus`/`fixtures/mix-corpus.txt` is where it
//! is certified: a transcript of every decision below over the space that
//! reaches it, captured from `userland/soundd/src/main.rs` before a line of it
//! moved here, and asserted byte for byte on every host run.
//!
//! **Audible behaviour is the owner's to change.** A change to the arithmetic
//! here reds the corpus, and that red is the design: it says a refactor altered
//! what a speaker plays, which is a thing nobody may do quietly.
//!
//! ## What this crate does not decide
//!
//! Resampling. A client at a rate the device does not run at is carried by
//! `rubato`, which is a third-party crate soundd holds and this one does not
//! name. What is here is everything on both sides of it — the decode that feeds
//! it ([`decode_i16_to_f32`]), the planar append that fills it
//! ([`append_planar`]), the interleave that empties it ([`interleave`]), and the
//! [`accumulate`] and [`quantize_period`] that carry its output to the wire —
//! plus [`client_period_frames`] and [`scratch_frames`], the two sizes whose
//! inequality is what keeps a resampling client inside the scratch the mix
//! thread allocated once.
//!
//! ## `no_std`, and the one thing that cost
//!
//! `core` has no `f32::round`, and the quantizer is a rounding quantizer:
//! [`round_ties_away`] is ours. It is not an approximation of the one it
//! replaces — `the_rounding_is_std_s_over_every_float_there_is` walks all 2^32
//! bit patterns and finds the quantizer's answer identical at every one of
//! them, with the only difference anywhere being that a NaN keeps its payload
//! here where `f32::round` canonicalises it, which no cast to `i16` can see.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod channel;
pub mod dll;
pub mod format;
pub mod gain;
pub mod mix;
pub mod shape;
pub mod stats;

#[cfg(test)]
mod corpus;

pub use channel::{
    append_planar, channel_convert_mono_to_stereo, channel_convert_stereo_to_mono, interleave,
};
pub use dll::Dll;
pub use format::{
    decode_i16_to_f32, dither_and_quantize, quantize, quantize_period, round_ties_away, Xorshift32,
    I16_SCALE,
};
pub use gain::{Gain, GainRamp};
pub use mix::{accumulate, mix_interleaved};
pub use shape::{
    client_period_frames, deferral_floor_nanos, period_frames, period_nanos, ramp_frames,
    scratch_frames, Shape, DEFERRAL_RESERVE, MAX_CLIENT_RATE, MAX_PIPELINE, MIN_CLIENT_RATE,
};
pub use stats::MixStats;
