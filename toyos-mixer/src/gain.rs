//! Volume: what a client is allowed to ask for, and how the answer moves.
//!
//! Nothing here steps a gain instantly. Every change soundd makes to a stream's
//! level — a connect, a disconnect, a `MSG_STREAM_SET_VOLUME` — is a ramp,
//! because a level that jumps between two samples is a click, and a click is
//! the one artefact a listener cannot fail to notice.

/// A gain that has already crossed the trust boundary: finite, and within
/// §7.4's [0.0, 1.0].
///
/// The check has to be a type rather than a `clamp` at each call site: `clamp`
/// returns NaN unchanged, and a NaN gain reaches the *shared* mix bus through
/// `accumulate`, silencing every stream.
#[derive(Clone, Copy)]
pub struct Gain(f32);

impl Gain {
    pub const SILENT: Gain = Gain(0.0);
    pub const UNITY: Gain = Gain(1.0);

    /// §7.4 clamps out-of-range values, and ±inf is out of range. NaN is not a
    /// value at all, so it is refused rather than guessed at; the control
    /// thread treats a refusal as any other malformed message.
    pub fn from_wire(gain: f32) -> Option<Gain> {
        if gain.is_nan() {
            return None;
        }
        Some(Gain(gain.clamp(0.0, 1.0)))
    }

    pub fn raw(self) -> f32 {
        self.0
    }
}

/// One stream's level, moving toward what was asked for.
pub struct GainRamp {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl GainRamp {
    pub fn new(initial: Gain) -> Self {
        Self { current: initial.raw(), target: initial.raw(), step: 0.0, remaining: 0 }
    }

    pub fn set_target(&mut self, target: Gain, ramp_frames: u32) {
        self.target = target.raw();
        self.step = (self.target - self.current) / ramp_frames as f32;
        self.remaining = ramp_frames;
    }

    /// Gain for the next frame (both channels of a frame get the same gain).
    // Not `Iterator`: a ramp does not end, it settles.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> f32 {
        if self.remaining > 0 {
            self.current += self.step;
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }

    /// Advance the ramp across a period of silence (§7.3: the ramp applies to
    /// silence when the ring is empty — otherwise a drained closing client
    /// would never finish its ramp and never be removed).
    pub fn advance_frames(&mut self, frames: u32) {
        let n = frames.min(self.remaining);
        self.current += self.step * n as f32;
        self.remaining -= n;
        if self.remaining == 0 {
            self.current = self.target;
        }
    }

    pub fn is_idle(&self) -> bool {
        self.remaining == 0
    }

    pub fn level(&self) -> f32 {
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **NaN is refused rather than clamped**, and this is the guard the whole
    /// mixer rests on: `clamp` returns NaN unchanged, `accumulate` multiplies
    /// the *shared* bus by it, and one client's malformed volume message would
    /// silence every stream on the machine.
    #[test]
    fn a_volume_that_is_not_a_number_is_refused_and_the_rest_is_clamped() {
        assert!(Gain::from_wire(f32::NAN).is_none());
        assert!(Gain::from_wire(-f32::NAN).is_none());
        assert_eq!(Gain::from_wire(f32::INFINITY).unwrap().raw(), 1.0);
        assert_eq!(Gain::from_wire(f32::NEG_INFINITY).unwrap().raw(), 0.0);
        assert_eq!(Gain::from_wire(2.0).unwrap().raw(), 1.0);
        assert_eq!(Gain::from_wire(-1.0).unwrap().raw(), 0.0);
        assert_eq!(Gain::from_wire(0.5).unwrap().raw(), 0.5);
        assert_eq!(Gain::from_wire(f32::MAX).unwrap().raw(), 1.0);
    }

    /// A ramp lands exactly on its target rather than near it: the accumulated
    /// step is a sum of `ramp_frames` roundings, and a level that settled at
    /// 0.9999997 instead of 1.0 would leave `is_idle` true and the gain wrong
    /// forever after.
    #[test]
    fn a_ramp_lands_exactly_on_its_target() {
        for frames in [1u32, 2, 3, 7, 220, 240, 960] {
            let mut ramp = GainRamp::new(Gain::SILENT);
            ramp.set_target(Gain::UNITY, frames);
            for _ in 0..frames {
                ramp.next();
            }
            assert!(ramp.is_idle(), "a {frames}-frame ramp had not finished");
            assert_eq!(ramp.level().to_bits(), 1.0f32.to_bits(), "{frames} frames");

            let mut ramp = GainRamp::new(Gain::UNITY);
            ramp.set_target(Gain::SILENT, frames);
            ramp.advance_frames(frames);
            assert!(ramp.is_idle());
            assert_eq!(ramp.level().to_bits(), 0.0f32.to_bits(), "{frames} frames, advanced");
        }
    }

    /// The ramp is monotone between its endpoints, so a fade never overshoots
    /// into a level louder than either end.
    #[test]
    fn a_fade_stays_between_its_endpoints() {
        let mut ramp = GainRamp::new(Gain::SILENT);
        ramp.set_target(Gain::UNITY, 220);
        let mut last = 0.0f32;
        for _ in 0..220 {
            let g = ramp.next();
            assert!((0.0..=1.0).contains(&g), "{g} left the range");
            assert!(g >= last, "{g} is below the previous {last}");
            last = g;
        }
    }

    /// **A settled ramp does not move.** `next` past the end is what the mix
    /// loop does every period of every stream that is not fading, and a level
    /// that drifted by a step per call would be a slow ramp nobody asked for.
    #[test]
    fn a_settled_ramp_holds_its_level() {
        let mut ramp = GainRamp::new(Gain::SILENT);
        ramp.set_target(Gain::UNITY, 4);
        for _ in 0..4 {
            ramp.next();
        }
        for _ in 0..1000 {
            assert_eq!(ramp.next().to_bits(), 1.0f32.to_bits());
        }
        ramp.advance_frames(10_000);
        assert_eq!(ramp.level().to_bits(), 1.0f32.to_bits());
    }

    /// Advancing past what is left settles rather than overshooting: a period is
    /// 128 frames and a ramp is 220, so the second silent period of a starved
    /// stream advances past the end of it.
    #[test]
    fn advancing_past_the_end_settles() {
        let mut ramp = GainRamp::new(Gain::UNITY);
        ramp.set_target(Gain::SILENT, 220);
        ramp.advance_frames(128);
        assert!(!ramp.is_idle());
        assert!(ramp.level() > 0.0 && ramp.level() < 1.0);
        ramp.advance_frames(128);
        assert!(ramp.is_idle());
        assert_eq!(ramp.level().to_bits(), 0.0f32.to_bits());
    }

    /// A ramp aimed at where it already is stays there — the volume message
    /// that changes nothing must not step the level at all.
    #[test]
    fn a_ramp_to_its_own_level_is_a_ramp_to_nowhere() {
        let mut ramp = GainRamp::new(Gain::UNITY);
        ramp.set_target(Gain::UNITY, 220);
        for _ in 0..300 {
            assert_eq!(ramp.next().to_bits(), 1.0f32.to_bits());
        }
    }

    /// **A retarget starts from where the ramp got to**, which is what makes a
    /// volume change during a connect fade continuous. It also stretches the
    /// remaining fade, which is exactly why `ClientStream::depart` refuses to
    /// re-aim a ramp already heading for silence.
    #[test]
    fn a_retarget_starts_from_the_level_reached() {
        let mut ramp = GainRamp::new(Gain::SILENT);
        ramp.set_target(Gain::UNITY, 200);
        for _ in 0..100 {
            ramp.next();
        }
        let mid = ramp.level();
        assert!((mid - 0.5).abs() < 1e-6, "{mid}");
        ramp.set_target(Gain::SILENT, 200);
        assert_eq!(ramp.next(), mid - mid / 200.0);
        for _ in 1..200 {
            ramp.next();
        }
        assert_eq!(ramp.level().to_bits(), 0.0f32.to_bits());
    }
}
