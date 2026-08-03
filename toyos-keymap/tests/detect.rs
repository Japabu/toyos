//! The layout wizard, driven by usage sequences instead of a person.
//!
//! The property that matters is not that the happy paths work — it is that
//! nothing produces a *confident wrong* verdict. `never_confident_without_
//! evidence` is that check, and it is exhaustive rather than sampled.

use toyos_keymap::detect::{Detector, Step, QUESTIONS};
use toyos_keymap::LAYOUTS;

/// Feed `usages` in order; return the verdict, or `None` for unrecognized.
///
/// A sequence that runs out before the wizard is done is an error in the test,
/// not an outcome: the wizard would still be asking.
fn run(usages: &[u8]) -> Option<&'static str> {
    let mut detector = Detector::new();
    let mut feed = usages.iter();
    loop {
        match detector.step() {
            Step::Decided(i) => return Some(LAYOUTS[i].name),
            Step::Unrecognized => return None,
            Step::Ask(ask) => {
                let usage = *feed.next().expect("ran out of presses while the wizard was asking");
                ask.observe(usage);
            }
        }
    }
}

/// The legends, in the order the wizard asks for them along `usages`.
fn legends(usages: &[u8]) -> Vec<&'static str> {
    let mut detector = Detector::new();
    let mut feed = usages.iter();
    let mut asked = Vec::new();
    loop {
        match detector.step() {
            Step::Decided(_) | Step::Unrecognized => return asked,
            Step::Ask(ask) => {
                asked.push(ask.legend());
                let Some(&usage) = feed.next() else { return asked };
                ask.observe(usage);
            }
        }
    }
}

#[test]
fn us_takes_one_press() {
    assert_eq!(run(&[0x1D]), Some("us"));
    assert_eq!(legends(&[0x1D]), ["Z"]);
}

#[test]
fn the_qwertz_three_take_two() {
    assert_eq!(run(&[0x1C, 0x20]), Some("de"));
    assert_eq!(run(&[0x1C, 0x35]), Some("swiss-german"));
    assert_eq!(run(&[0x1C, 0x64]), Some("swiss-german-mac"));
    assert_eq!(legends(&[0x1C, 0x35]), ["Z", "\u{a7}"]);
}

/// Every layout the machine has must be reachable, or the wizard can never
/// select it — derived from the table rather than from the cases above, so a
/// fifth layout with no distinguishing row fails here.
#[test]
fn every_layout_is_reachable() {
    for layout in LAYOUTS {
        let mut detector = Detector::new();
        let mut pressed = Vec::new();
        let verdict = loop {
            match detector.step() {
                Step::Decided(i) => break Some(LAYOUTS[i].name),
                Step::Unrecognized => break None,
                Step::Ask(ask) => {
                    let legend = ask.legend();
                    let q = QUESTIONS.iter().find(|q| q.legend == legend).expect("asked legend");
                    let &(_, usage) = q
                        .answers
                        .iter()
                        .find(|&&(name, _)| name == layout.name)
                        .unwrap_or_else(|| panic!("{} unanswered for {legend}", layout.name));
                    pressed.push(usage);
                    ask.observe(usage);
                }
            }
        };
        assert_eq!(verdict, Some(layout.name), "pressing {pressed:02x?} did not identify it");
        assert!(pressed.len() <= 2, "{} took {} presses", layout.name, pressed.len());
    }
}

// --- negative controls ---

/// A first press no layout puts that legend on.
#[test]
fn inconsistent_first_press_is_unrecognized() {
    for usage in [0x04, 0x07, 0x2C, 0x00, 0xE1] {
        assert_eq!(run(&[usage]), None, "usage {usage:#04x} produced a verdict");
    }
}

/// A first press that is consistent, then one that is not. The wizard has a
/// live candidate set at that point and must still refuse rather than fall
/// back to it.
#[test]
fn inconsistent_second_press_is_unrecognized() {
    for usage in [0x07, 0x1C, 0x1D, 0x2C, 0x31] {
        assert_eq!(run(&[0x1C, usage]), None, "0x1C then {usage:#04x} produced a verdict");
    }
}

/// Exhaustive: no sequence of two presses names a layout that does not put
/// both legends where they were pressed.
#[test]
fn never_confident_without_evidence() {
    let mut verdicts = 0;
    for first in 0..=u8::MAX {
        for second in 0..=u8::MAX {
            let Some(name) = run(&[first, second]) else { continue };
            verdicts += 1;
            // Replay: every question the wizard asked must have an answer for
            // the layout it named, at the usage that was actually pressed.
            let mut detector = Detector::new();
            let mut feed = [first, second].into_iter();
            loop {
                match detector.step() {
                    Step::Decided(i) => {
                        assert_eq!(LAYOUTS[i].name, name);
                        break;
                    }
                    Step::Unrecognized => unreachable!("it decided a moment ago"),
                    Step::Ask(ask) => {
                        let legend = ask.legend();
                        let usage = feed.next().expect("two presses");
                        let q = QUESTIONS.iter().find(|q| q.legend == legend).unwrap();
                        assert!(
                            q.answers.contains(&(name, usage)),
                            "{name} named after {legend} was pressed at {usage:#04x}, \
                             which is not where {name} puts it",
                        );
                        ask.observe(usage);
                    }
                }
            }
        }
    }
    // A guard on the guard: a detector that never decided anything would pass
    // the loop above vacuously.
    assert!(verdicts > 0, "no sequence ever produced a verdict");
}

/// The wizard must not need a glyph the console cannot draw. Its font covers
/// Latin-1; anything above renders as `?`.
#[test]
fn legends_are_renderable() {
    for q in QUESTIONS {
        assert!(!q.legend.is_empty(), "a question with no legend");
        for ch in q.legend.chars() {
            assert!(
                (ch as u32) <= 0xFF,
                "legend {:?} needs U+{:04X}, outside the console font",
                q.legend,
                ch as u32
            );
        }
    }
}

#[test]
fn question_table_is_well_formed() {
    for q in QUESTIONS {
        let mut seen: Vec<&str> = Vec::new();
        for &(name, _) in q.answers {
            assert!(
                LAYOUTS.iter().any(|l| l.name == name),
                "question {:?} answers for {name}, which is not a layout",
                q.legend
            );
            assert!(!seen.contains(&name), "question {:?} names {name} twice", q.legend);
            seen.push(name);
        }
    }
}

/// The wizard reports what is still open, so an unrecognized run can say what
/// it was choosing between.
#[test]
fn candidates_narrow() {
    let mut detector = Detector::new();
    assert_eq!(detector.candidates().count(), LAYOUTS.len());
    match detector.step() {
        Step::Ask(ask) => ask.observe(0x1C),
        _ => panic!("expected a question"),
    }
    let left: Vec<&str> = detector.candidates().collect();
    assert_eq!(left, ["de", "swiss-german", "swiss-german-mac"]);
}
