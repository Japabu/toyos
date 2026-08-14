//! doom's music bank: a General MIDI SoundFont cut down to what doom asks for.
//!
//! `assets/soundfont.sf2` is not a file somebody found. It is [`subset`] run
//! over GeneralUser GS with the instrument list [`doom_instruments`] reads out
//! of `assets/DOOM1.WAD`, and `cargo run -- --regen-soundfont <bank.sf2>`
//! rewrites it. The `src/wallpaper.rs` precedent: a committed producer makes the
//! artifact reproducible, lets the instrument list move without an agent, and
//! says the blob is derived rather than opaque.
//!
//! **The source bank is not committed and the derivation is therefore not
//! byte-checkable the way the wallpaper's is** — 32,319,396 bytes to prove a
//! property of 15,546,748. What is checkable without it is the property that
//! matters, and the tests below check exactly that: the shipped file covers
//! every instrument the WAD selects, carries nothing else, and still has the
//! source's copyright and licence text inside it.
//!
//! A MUS lump names its instruments in its own header, so what doom needs is
//! read rather than guessed, and one function answers it for both the producer
//! and the gate. Deleting a preset doom never selects costs nothing audible:
//! all 13 tracks render bit-exact against the full bank
//! (`specs/assessments/doom-music-soundfont.md` §4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The shipped bank, relative to the repository root.
pub const SOUNDFONT_PATH: &str = "assets/soundfont.sf2";

/// The WAD whose MUS lumps decide what the bank must contain.
pub const WAD_PATH: &str = "assets/DOOM1.WAD";

/// The source bank's own licence text, which `NOTICE` points at. It is the
/// shipped file's `ICMT` chunk, read back out of it.
pub const LICENCE_PATH: &str = "licenses/GeneralUser-GS-License-v2.0.txt";

/// The name `INAM` gets, so the file says what it is when a synth lists it.
const SUBSET_NAME: &str = "ToyOS Doom GM subset";

/// `ISFT` is "the tool that wrote this file", and this is that tool.
const SUBSET_TOOL: &str = "toyos-build --regen-soundfont";

/// SF2 generator operators, from the SoundFont 2.04 spec §8.1.
const GEN_INSTRUMENT: u16 = 41;
const GEN_KEYRANGE: u16 = 43;
const GEN_SAMPLEID: u16 = 53;

/// Record widths in `pdta`, spec §7.
const PHDR_LEN: usize = 38;
const BAG_LEN: usize = 4;
const MOD_LEN: usize = 10;
const GEN_LEN: usize = 4;
const INST_LEN: usize = 22;
const SHDR_LEN: usize = 46;

/// A sample's `sfSampleType`: mono, then the two halves of a stereo pair.
const SAMPLE_MONO: u16 = 1;
const SAMPLE_RIGHT: u16 = 2;
const SAMPLE_LEFT: u16 = 4;
/// Set in `sfSampleType` when the sample lives in ROM rather than in `smpl`.
const SAMPLE_ROM: u16 = 0x8000;

/// Zero frames every written sample is followed by, spec §7.10: a synth may
/// read past the end while an interpolator drains.
const SAMPLE_GUARD_FRAMES: usize = 46;

/// The GM percussion bank. A MUS percussion instrument is `100 + key`, so this
/// is also what separates the two halves of a MUS instrument list.
const DRUM_BANK: u16 = 128;
const MUS_PERCUSSION_BASE: u16 = 100;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

// ── What doom asks for ──────────────────────────────────────────────────────

/// The instruments doom's music selects: General MIDI melodic programs, and
/// percussion keys on the drum channel.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Instruments {
    pub melodic: BTreeSet<u16>,
    pub percussion: BTreeSet<u8>,
}

/// The lumps of a WAD, in directory order.
///
/// Header is `IWAD`/`PWAD`, a lump count and the directory offset; each entry
/// is a position, a length and an eight-byte name (spec: the Unofficial Doom
/// Specs §2). A length or offset that runs off the end is a WAD this build has
/// no business reading, so it stops.
fn wad_lumps(wad: &[u8]) -> Vec<(String, &[u8])> {
    assert!(wad.len() >= 12, "not a WAD: {} bytes", wad.len());
    let magic = &wad[0..4];
    assert!(magic == b"IWAD" || magic == b"PWAD", "not a WAD: magic {magic:?}");
    let count = u32le(wad, 4) as usize;
    let directory = u32le(wad, 8) as usize;

    (0..count)
        .map(|i| {
            let entry = directory + i * 16;
            assert!(entry + 16 <= wad.len(), "WAD directory entry {i} is past the end");
            let start = u32le(wad, entry) as usize;
            let len = u32le(wad, entry + 4) as usize;
            assert!(start + len <= wad.len(), "WAD lump {i} runs past the end");
            let name: String = wad[entry + 8..entry + 16]
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as char)
                .collect();
            (name, &wad[start..start + len])
        })
        .collect()
}

/// Every instrument every MUS lump in `wad` declares.
///
/// A MUS header carries its own instrument list — count at offset 12, the list
/// at 16 — so this is an enumeration and not an estimate. Values below 128 are
/// GM programs; the rest are `100 + key` on the percussion channel.
pub fn doom_instruments(wad: &[u8]) -> Instruments {
    let mut want = Instruments::default();
    for (name, lump) in wad_lumps(wad) {
        if !lump.starts_with(b"MUS\x1a") {
            continue;
        }
        assert!(lump.len() >= 16, "MUS lump {name} is {} bytes", lump.len());
        let count = u16le(lump, 12) as usize;
        assert!(16 + count * 2 <= lump.len(), "MUS lump {name} truncates its instrument list");
        for i in 0..count {
            let instrument = u16le(lump, 16 + i * 2);
            match instrument {
                0..=127 => {
                    want.melodic.insert(instrument);
                }
                _ => {
                    let key = instrument - MUS_PERCUSSION_BASE;
                    let key = u8::try_from(key)
                        .unwrap_or_else(|_| panic!("MUS lump {name}: percussion key {key}"));
                    want.percussion.insert(key);
                }
            }
        }
    }
    assert!(!want.melodic.is_empty(), "no MUS lump in this WAD");
    want
}

// ── Reading a bank ──────────────────────────────────────────────────────────

struct Preset {
    name: [u8; 20],
    preset: u16,
    bank: u16,
    bag: u16,
    library: u32,
    genre: u32,
    morphology: u32,
}

struct Instrument {
    name: [u8; 20],
    bag: u16,
}

#[derive(Clone)]
struct Sample {
    name: [u8; 20],
    start: u32,
    end: u32,
    start_loop: u32,
    end_loop: u32,
    rate: u32,
    pitch: u8,
    correction: i8,
    link: u16,
    kind: u16,
}

/// An SF2 file, decoded far enough to rewrite it.
///
/// `info` stays as raw chunks: the copyright and the licence live in there and
/// must survive into a derivative unread rather than be re-stated by us.
struct Bank {
    info: Vec<([u8; 4], Vec<u8>)>,
    smpl: Vec<u8>,
    sm24: Vec<u8>,
    presets: Vec<Preset>,
    /// `(generator index, modulator index)` per bag, both `pbag` and `ibag`.
    preset_bags: Vec<(u16, u16)>,
    preset_mods: Vec<u8>,
    preset_gens: Vec<(u16, u16)>,
    instruments: Vec<Instrument>,
    instrument_bags: Vec<(u16, u16)>,
    instrument_mods: Vec<u8>,
    instrument_gens: Vec<(u16, u16)>,
    samples: Vec<Sample>,
}

/// The `(id, offset, length)` of every chunk between `at` and `end`.
fn chunks(b: &[u8], mut at: usize, end: usize) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    while at + 8 <= end {
        let id = [b[at], b[at + 1], b[at + 2], b[at + 3]];
        let size = u32le(b, at + 4) as usize;
        assert!(at + 8 + size <= end, "chunk {:?} runs past its parent", String::from_utf8_lossy(&id));
        out.push((id, at + 8, size));
        at += 8 + size + (size & 1);
    }
    out
}

impl Bank {
    fn read(b: &[u8]) -> Bank {
        assert!(b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"sfbk", "not an SF2 file");

        let mut info = Vec::new();
        let mut smpl = Vec::new();
        let mut sm24 = Vec::new();
        let mut pdta: BTreeMap<[u8; 4], (usize, usize)> = BTreeMap::new();

        for (id, at, len) in chunks(b, 12, 8 + u32le(b, 4) as usize) {
            if &id != b"LIST" {
                continue;
            }
            let kind = [b[at], b[at + 1], b[at + 2], b[at + 3]];
            let inner = chunks(b, at + 4, at + len);
            match &kind {
                b"INFO" => {
                    info = inner.iter().map(|&(id, at, len)| (id, b[at..at + len].to_vec())).collect()
                }
                b"sdta" => {
                    for (id, at, len) in inner {
                        match &id {
                            b"smpl" => smpl = b[at..at + len].to_vec(),
                            b"sm24" => sm24 = b[at..at + len].to_vec(),
                            _ => {}
                        }
                    }
                }
                b"pdta" => {
                    for (id, at, len) in inner {
                        pdta.insert(id, (at, len));
                    }
                }
                _ => {}
            }
        }

        let table = |id: &[u8; 4]| -> (usize, usize) {
            *pdta.get(id).unwrap_or_else(|| {
                panic!("SF2 has no {} chunk", String::from_utf8_lossy(id))
            })
        };
        let records = |id: &[u8; 4], width: usize| -> (usize, usize) {
            let (at, len) = table(id);
            assert_eq!(
                len % width,
                0,
                "{} is {len} bytes, not a whole number of {width}-byte records",
                String::from_utf8_lossy(id)
            );
            (at, len / width)
        };
        let bags = |id: &[u8; 4]| -> Vec<(u16, u16)> {
            let (at, count) = records(id, BAG_LEN);
            (0..count).map(|i| (u16le(b, at + i * 4), u16le(b, at + i * 4 + 2))).collect()
        };
        let gens = |id: &[u8; 4]| -> Vec<(u16, u16)> {
            let (at, count) = records(id, GEN_LEN);
            (0..count).map(|i| (u16le(b, at + i * 4), u16le(b, at + i * 4 + 2))).collect()
        };
        let raw = |id: &[u8; 4]| -> Vec<u8> {
            let (at, len) = table(id);
            b[at..at + len].to_vec()
        };
        let name_at = |at: usize| -> [u8; 20] {
            let mut name = [0u8; 20];
            name.copy_from_slice(&b[at..at + 20]);
            name
        };

        let (phdr, preset_count) = records(b"phdr", PHDR_LEN);
        let (inst, instrument_count) = records(b"inst", INST_LEN);
        let (shdr, sample_count) = records(b"shdr", SHDR_LEN);

        Bank {
            info,
            smpl,
            sm24,
            presets: (0..preset_count)
                .map(|i| {
                    let o = phdr + i * PHDR_LEN;
                    Preset {
                        name: name_at(o),
                        preset: u16le(b, o + 20),
                        bank: u16le(b, o + 22),
                        bag: u16le(b, o + 24),
                        library: u32le(b, o + 26),
                        genre: u32le(b, o + 30),
                        morphology: u32le(b, o + 34),
                    }
                })
                .collect(),
            preset_bags: bags(b"pbag"),
            preset_mods: raw(b"pmod"),
            preset_gens: gens(b"pgen"),
            instruments: (0..instrument_count)
                .map(|i| {
                    let o = inst + i * INST_LEN;
                    Instrument { name: name_at(o), bag: u16le(b, o + 20) }
                })
                .collect(),
            instrument_bags: bags(b"ibag"),
            instrument_mods: raw(b"imod"),
            instrument_gens: gens(b"igen"),
            samples: (0..sample_count)
                .map(|i| {
                    let o = shdr + i * SHDR_LEN;
                    Sample {
                        name: name_at(o),
                        start: u32le(b, o + 20),
                        end: u32le(b, o + 24),
                        start_loop: u32le(b, o + 28),
                        end_loop: u32le(b, o + 32),
                        rate: u32le(b, o + 36),
                        pitch: b[o + 40],
                        correction: b[o + 41] as i8,
                        link: u16le(b, o + 42),
                        kind: u16le(b, o + 44),
                    }
                })
                .collect(),
        }
    }

    /// The generators of bag `bag`, whichever table it came from.
    fn zone<'a>(bags: &[(u16, u16)], gens: &'a [(u16, u16)], bag: usize) -> &'a [(u16, u16)] {
        &gens[bags[bag].0 as usize..bags[bag + 1].0 as usize]
    }
}

/// The bank/preset pairs a file declares, terminal `EOP` record excluded.
pub fn preset_numbers(sf2: &[u8]) -> BTreeSet<(u16, u16)> {
    let bank = Bank::read(sf2);
    bank.presets[..bank.presets.len() - 1].iter().map(|p| (p.bank, p.preset)).collect()
}

/// The keys a zone's `keyRange` admits — the whole keyboard when it has none.
fn key_range(gens: &[(u16, u16)]) -> (u8, u8) {
    match gens.iter().find(|(op, _)| *op == GEN_KEYRANGE) {
        Some(&(_, range)) => ((range & 0xff) as u8, (range >> 8) as u8),
        None => (0, 127),
    }
}

/// Every percussion key `sf2`'s bank-128 preset 0 can sound.
///
/// **Both levels restrict, so a zone sounds their intersection.** GeneralUser
/// GS's kit uses each for about half of it — kick and snare carry the key on
/// the *preset* zone with velocity layers under it, toms and hi-hats carry it
/// on the instrument zones — so reading one level answers "all 128" for half
/// the kit, which is no gate at all.
pub fn drum_keys(sf2: &[u8]) -> BTreeSet<u8> {
    let bank = Bank::read(sf2);
    let mut keys = BTreeSet::new();
    for (i, preset) in bank.presets[..bank.presets.len() - 1].iter().enumerate() {
        if preset.bank != DRUM_BANK || preset.preset != 0 {
            continue;
        }
        for bag in preset.bag as usize..bank.presets[i + 1].bag as usize {
            let gens = Bank::zone(&bank.preset_bags, &bank.preset_gens, bag);
            let Some(&(_, instrument)) = gens.iter().find(|(op, _)| *op == GEN_INSTRUMENT) else {
                continue;
            };
            let (preset_lo, preset_hi) = key_range(gens);
            let instrument = instrument as usize;
            for bag in bank.instruments[instrument].bag as usize
                ..bank.instruments[instrument + 1].bag as usize
            {
                let gens = Bank::zone(&bank.instrument_bags, &bank.instrument_gens, bag);
                // A zone with no sample is the instrument's global zone: it
                // sets defaults for the others and sounds nothing itself.
                if !gens.iter().any(|(op, _)| *op == GEN_SAMPLEID) {
                    continue;
                }
                let (lo, hi) = key_range(gens);
                keys.extend(preset_lo.max(lo)..=preset_hi.min(hi));
            }
        }
    }
    keys
}

/// The `INFO` chunk `id` carries, as text.
pub fn info_text(sf2: &[u8], id: &[u8; 4]) -> Option<String> {
    Bank::read(sf2).info.into_iter().find(|(chunk, _)| chunk == id).map(|(_, body)| {
        String::from_utf8_lossy(&body).trim_end_matches('\0').to_string()
    })
}

// ── Writing the subset ──────────────────────────────────────────────────────

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len() + 1);
    out.extend_from_slice(id);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    if body.len() & 1 == 1 {
        out.push(0);
    }
    out
}

/// An `INFO` string: NUL-terminated and an even number of bytes, spec §5.
fn zstring(s: &str) -> Vec<u8> {
    let mut out = s.as_bytes().to_vec();
    out.push(0);
    if out.len() & 1 == 1 {
        out.push(0);
    }
    out
}

/// The name of a terminal `EOP`/`EOI`/`EOS` record. Every `pdta` table ends
/// with one whose indices bound the last real record's span, spec §7.
fn terminal(name: &str) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[..name.len()].copy_from_slice(name.as_bytes());
    out
}

/// A zone survives the drum pass if it can sound one of the wanted keys.
///
/// Applied only inside bank 128: a melodic preset's key ranges are how one
/// instrument is spread across the keyboard, and cutting those would change
/// what the instruments doom *does* use sound like.
fn sounds_a_wanted_key(gens: &[(u16, u16)], want: &BTreeSet<u8>) -> bool {
    let (lo, hi) = key_range(gens);
    want.iter().any(|&key| key >= lo && key <= hi)
}

/// `source` with everything doom never selects taken out.
///
/// Keeps the melodic presets `want` names in bank 0 and the bank-128 drum kit,
/// prunes the kit's zones to the keys `want` names, then rebuilds every `pdta`
/// table and the sample pool around what survived. The source's `INFO` chunks
/// are carried over unread apart from the two that name the file itself, so the
/// copyright and the licence travel with the derivative by construction rather
/// than by anybody remembering to copy them.
pub fn subset(source: &[u8], want: &Instruments) -> Vec<u8> {
    let bank = Bank::read(source);

    let keep: Vec<usize> = (0..bank.presets.len() - 1)
        .filter(|&i| {
            let p = &bank.presets[i];
            (p.bank == 0 && want.melodic.contains(&p.preset))
                || (p.bank == DRUM_BANK && p.preset == 0)
        })
        .collect();
    let missing: Vec<u16> = want
        .melodic
        .iter()
        .copied()
        .filter(|program| {
            !keep.iter().any(|&i| bank.presets[i].bank == 0 && bank.presets[i].preset == *program)
        })
        .collect();
    assert!(missing.is_empty(), "this bank has no melodic program {missing:?}, which doom selects");

    // Presets first: which instruments survive is decided by which preset zones
    // survive, and which samples by which instrument zones do.
    struct Kept {
        source: usize,
        zones: Vec<(usize, Vec<(u16, u16)>)>,
    }
    let mut instrument_order: Vec<usize> = Vec::new();
    let mut instrument_seen: BTreeSet<usize> = BTreeSet::new();
    let mut kept_presets: Vec<Kept> = Vec::new();

    for &i in &keep {
        let drums = bank.presets[i].bank == DRUM_BANK;
        let mut zones = Vec::new();
        for bag in bank.presets[i].bag as usize..bank.presets[i + 1].bag as usize {
            let gens = Bank::zone(&bank.preset_bags, &bank.preset_gens, bag).to_vec();
            if drums && !sounds_a_wanted_key(&gens, &want.percussion) {
                continue;
            }
            if let Some(&(_, instrument)) = gens.iter().find(|(op, _)| *op == GEN_INSTRUMENT) {
                if instrument_seen.insert(instrument as usize) {
                    instrument_order.push(instrument as usize);
                }
            }
            zones.push((bag, gens));
        }
        kept_presets.push(Kept { source: i, zones });
    }

    let drum_instruments: BTreeSet<usize> = kept_presets
        .iter()
        .filter(|kept| bank.presets[kept.source].bank == DRUM_BANK)
        .flat_map(|kept| &kept.zones)
        .filter_map(|(_, gens)| gens.iter().find(|(op, _)| *op == GEN_INSTRUMENT))
        .map(|&(_, instrument)| instrument as usize)
        .collect();

    let mut sample_order: Vec<usize> = Vec::new();
    let mut sample_seen: BTreeSet<usize> = BTreeSet::new();
    let mut kept_instruments: Vec<Kept> = Vec::new();

    for &i in &instrument_order {
        let drums = drum_instruments.contains(&i);
        let mut zones = Vec::new();
        for bag in bank.instruments[i].bag as usize..bank.instruments[i + 1].bag as usize {
            let gens = Bank::zone(&bank.instrument_bags, &bank.instrument_gens, bag).to_vec();
            if drums && !sounds_a_wanted_key(&gens, &want.percussion) {
                continue;
            }
            if let Some(&(_, sample)) = gens.iter().find(|(op, _)| *op == GEN_SAMPLEID) {
                if sample_seen.insert(sample as usize) {
                    sample_order.push(sample as usize);
                }
            }
            zones.push((bag, gens));
        }
        kept_instruments.push(Kept { source: i, zones });
    }

    // A stereo half whose partner was left behind has a dangling link, so the
    // partner comes too.
    let partners: Vec<usize> = sample_order
        .iter()
        .map(|&i| &bank.samples[i])
        .filter(|s| s.kind & SAMPLE_ROM == 0 && (s.kind == SAMPLE_RIGHT || s.kind == SAMPLE_LEFT))
        .map(|s| s.link as usize)
        .filter(|&link| link < bank.samples.len() - 1 && !sample_seen.contains(&link))
        .collect();
    for link in partners {
        if sample_seen.insert(link) {
            sample_order.push(link);
        }
    }

    // ── the sample pool ──
    let has24 = !bank.sm24.is_empty();
    let mut smpl: Vec<u8> = Vec::new();
    let mut sm24: Vec<u8> = Vec::new();
    let mut sample_index: BTreeMap<usize, u16> = BTreeMap::new();
    let mut samples: Vec<Sample> = Vec::new();

    for (to, &from) in sample_order.iter().enumerate() {
        let s = &bank.samples[from];
        let base = (smpl.len() / 2) as u32;
        let frames = s.end - s.start;
        smpl.extend_from_slice(&bank.smpl[s.start as usize * 2..s.end as usize * 2]);
        smpl.extend(std::iter::repeat(0u8).take(SAMPLE_GUARD_FRAMES * 2));
        if has24 {
            sm24.extend_from_slice(&bank.sm24[s.start as usize..s.end as usize]);
            sm24.extend(std::iter::repeat(0u8).take(SAMPLE_GUARD_FRAMES));
        }
        samples.push(Sample {
            start: base,
            end: base + frames,
            start_loop: base + (s.start_loop - s.start),
            end_loop: base + (s.end_loop - s.start),
            ..s.clone()
        });
        sample_index.insert(from, to as u16);
    }
    for (to, &from) in sample_order.iter().enumerate() {
        let stereo = samples[to].kind == SAMPLE_RIGHT || samples[to].kind == SAMPLE_LEFT;
        match sample_index.get(&(bank.samples[from].link as usize)) {
            Some(&link) if stereo => samples[to].link = link,
            _ => {
                samples[to].link = 0;
                if stereo {
                    samples[to].kind = SAMPLE_MONO;
                }
            }
        }
    }
    if sm24.len() & 1 == 1 {
        sm24.push(0);
    }

    // ── the tables ──
    let instrument_index: BTreeMap<usize, u16> =
        kept_instruments.iter().enumerate().map(|(to, k)| (k.source, to as u16)).collect();

    let mut out_instruments: Vec<Instrument> = Vec::new();
    let mut out_instrument_bags: Vec<(u16, u16)> = Vec::new();
    let mut out_instrument_gens: Vec<(u16, u16)> = Vec::new();
    let mut out_instrument_mods: Vec<u8> = Vec::new();

    for kept in &kept_instruments {
        out_instruments.push(Instrument {
            name: bank.instruments[kept.source].name,
            bag: out_instrument_bags.len() as u16,
        });
        for (bag, gens) in &kept.zones {
            out_instrument_bags
                .push((out_instrument_gens.len() as u16, (out_instrument_mods.len() / MOD_LEN) as u16));
            for &(op, amount) in gens {
                let amount = if op == GEN_SAMPLEID {
                    *sample_index.get(&(amount as usize)).unwrap_or(&0)
                } else {
                    amount
                };
                out_instrument_gens.push((op, amount));
            }
            let mods = bank.instrument_bags[*bag].1 as usize..bank.instrument_bags[bag + 1].1 as usize;
            out_instrument_mods
                .extend_from_slice(&bank.instrument_mods[mods.start * MOD_LEN..mods.end * MOD_LEN]);
        }
    }
    out_instruments.push(Instrument { name: terminal("EOI"), bag: out_instrument_bags.len() as u16 });
    out_instrument_bags
        .push((out_instrument_gens.len() as u16, (out_instrument_mods.len() / MOD_LEN) as u16));
    out_instrument_gens.push((0, 0));
    out_instrument_mods.extend_from_slice(&[0u8; MOD_LEN]);

    let mut out_presets: Vec<Preset> = Vec::new();
    let mut out_preset_bags: Vec<(u16, u16)> = Vec::new();
    let mut out_preset_gens: Vec<(u16, u16)> = Vec::new();
    let mut out_preset_mods: Vec<u8> = Vec::new();

    for kept in &kept_presets {
        let source = &bank.presets[kept.source];
        out_presets.push(Preset {
            name: source.name,
            preset: source.preset,
            bank: source.bank,
            bag: out_preset_bags.len() as u16,
            library: source.library,
            genre: source.genre,
            morphology: source.morphology,
        });
        for (bag, gens) in &kept.zones {
            out_preset_bags
                .push((out_preset_gens.len() as u16, (out_preset_mods.len() / MOD_LEN) as u16));
            for &(op, amount) in gens {
                let amount = if op == GEN_INSTRUMENT {
                    *instrument_index.get(&(amount as usize)).unwrap_or(&0)
                } else {
                    amount
                };
                out_preset_gens.push((op, amount));
            }
            let mods = bank.preset_bags[*bag].1 as usize..bank.preset_bags[bag + 1].1 as usize;
            out_preset_mods
                .extend_from_slice(&bank.preset_mods[mods.start * MOD_LEN..mods.end * MOD_LEN]);
        }
    }
    out_presets.push(Preset {
        name: terminal("EOP"),
        preset: 0,
        bank: 0,
        bag: out_preset_bags.len() as u16,
        library: 0,
        genre: 0,
        morphology: 0,
    });
    out_preset_bags.push((out_preset_gens.len() as u16, (out_preset_mods.len() / MOD_LEN) as u16));
    out_preset_gens.push((0, 0));
    out_preset_mods.extend_from_slice(&[0u8; MOD_LEN]);

    samples.push(Sample {
        name: terminal("EOS"),
        start: 0,
        end: 0,
        start_loop: 0,
        end_loop: 0,
        rate: 0,
        pitch: 0,
        correction: 0,
        link: 0,
        kind: 0,
    });

    // ── serialise ──
    let mut info: Vec<u8> = b"INFO".to_vec();
    for (id, body) in &bank.info {
        let body = match id {
            b"INAM" => zstring(SUBSET_NAME),
            b"ISFT" => zstring(SUBSET_TOOL),
            _ => body.clone(),
        };
        info.extend_from_slice(&chunk(id, &body));
    }

    let mut sdta: Vec<u8> = b"sdta".to_vec();
    sdta.extend_from_slice(&chunk(b"smpl", &smpl));
    if has24 {
        sdta.extend_from_slice(&chunk(b"sm24", &sm24));
    }

    let pairs = |v: &[(u16, u16)]| -> Vec<u8> {
        v.iter().flat_map(|(a, b)| [a.to_le_bytes(), b.to_le_bytes()].concat()).collect()
    };
    let mut pdta: Vec<u8> = b"pdta".to_vec();
    pdta.extend_from_slice(&chunk(
        b"phdr",
        &out_presets
            .iter()
            .flat_map(|p| {
                let mut r = p.name.to_vec();
                r.extend_from_slice(&p.preset.to_le_bytes());
                r.extend_from_slice(&p.bank.to_le_bytes());
                r.extend_from_slice(&p.bag.to_le_bytes());
                r.extend_from_slice(&p.library.to_le_bytes());
                r.extend_from_slice(&p.genre.to_le_bytes());
                r.extend_from_slice(&p.morphology.to_le_bytes());
                r
            })
            .collect::<Vec<u8>>(),
    ));
    pdta.extend_from_slice(&chunk(b"pbag", &pairs(&out_preset_bags)));
    pdta.extend_from_slice(&chunk(b"pmod", &out_preset_mods));
    pdta.extend_from_slice(&chunk(b"pgen", &pairs(&out_preset_gens)));
    pdta.extend_from_slice(&chunk(
        b"inst",
        &out_instruments
            .iter()
            .flat_map(|i| {
                let mut r = i.name.to_vec();
                r.extend_from_slice(&i.bag.to_le_bytes());
                r
            })
            .collect::<Vec<u8>>(),
    ));
    pdta.extend_from_slice(&chunk(b"ibag", &pairs(&out_instrument_bags)));
    pdta.extend_from_slice(&chunk(b"imod", &out_instrument_mods));
    pdta.extend_from_slice(&chunk(b"igen", &pairs(&out_instrument_gens)));
    pdta.extend_from_slice(&chunk(
        b"shdr",
        &samples
            .iter()
            .flat_map(|s| {
                let mut r = s.name.to_vec();
                r.extend_from_slice(&s.start.to_le_bytes());
                r.extend_from_slice(&s.end.to_le_bytes());
                r.extend_from_slice(&s.start_loop.to_le_bytes());
                r.extend_from_slice(&s.end_loop.to_le_bytes());
                r.extend_from_slice(&s.rate.to_le_bytes());
                r.push(s.pitch);
                r.push(s.correction as u8);
                r.extend_from_slice(&s.link.to_le_bytes());
                r.extend_from_slice(&s.kind.to_le_bytes());
                r
            })
            .collect::<Vec<u8>>(),
    ));

    let mut body: Vec<u8> = b"sfbk".to_vec();
    body.extend_from_slice(&chunk(b"LIST", &info));
    body.extend_from_slice(&chunk(b"LIST", &sdta));
    body.extend_from_slice(&chunk(b"LIST", &pdta));

    let mut out: Vec<u8> = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Rewrite [`SOUNDFONT_PATH`] from `source`, which is the whole bank.
///
/// The whole bank is not in this repository — it is 32,319,396 bytes to the
/// subset's 15,546,764, and shipping both would be shipping the same samples
/// twice. GeneralUser GS v2.0.3 comes from <https://www.schristiancollins.com>;
/// `NOTICE` says what its licence is and what the caveat on it is.
pub fn regen(root: &Path, source: &Path) {
    let bank = std::fs::read(source)
        .unwrap_or_else(|e| panic!("--regen-soundfont: read {}: {e}", source.display()));
    let wad = std::fs::read(root.join(WAD_PATH))
        .unwrap_or_else(|e| panic!("--regen-soundfont: read {WAD_PATH}: {e}"));

    let want = doom_instruments(&wad);
    let out = subset(&bank, &want);
    let path = root.join(SOUNDFONT_PATH);
    std::fs::write(&path, &out).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));

    println!(
        "{} selects {} melodic programs and {} percussion keys",
        WAD_PATH,
        want.melodic.len(),
        want.percussion.len()
    );
    println!(
        "wrote {} ({} bytes, from {} bytes of {})",
        path.display(),
        out.len(),
        bank.len(),
        source.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    fn shipped() -> Vec<u8> {
        std::fs::read(root().join(SOUNDFONT_PATH)).unwrap_or_else(|e| {
            panic!("{SOUNDFONT_PATH}: {e} — doom's music is not in this tree")
        })
    }

    fn wanted() -> Instruments {
        doom_instruments(&std::fs::read(root().join(WAD_PATH)).expect("read the WAD"))
    }

    /// The gate the removal of the last SoundFont did not have.
    ///
    /// `b8b0749` took doom's music out and nothing said so for a cycle. What
    /// would have caught it is this: the shipped bank is asked whether it can
    /// sound every instrument the shipped WAD selects, with both halves read
    /// from the files that ship rather than from a list somebody typed. A
    /// missing file, a truncated one, a bank subset against a different WAD and
    /// a WAD whose music changed all land here.
    #[test]
    fn the_shipped_bank_sounds_every_instrument_the_shipped_wad_selects() {
        let want = wanted();
        let sf2 = shipped();

        let melodic: BTreeSet<u16> = preset_numbers(&sf2)
            .iter()
            .filter(|(bank, _)| *bank == 0)
            .map(|(_, preset)| *preset)
            .collect();
        assert_eq!(
            want.melodic.difference(&melodic).copied().collect::<Vec<u16>>(),
            Vec::<u16>::new(),
            "doom selects melodic programs this bank does not have"
        );

        let keys = drum_keys(&sf2);
        assert_eq!(
            want.percussion.difference(&keys).copied().collect::<Vec<u8>>(),
            Vec::<u8>::new(),
            "doom selects percussion keys this bank's drum kit cannot sound"
        );
    }

    /// And nothing else, which is the other half of "derived".
    ///
    /// Without this the test above passes on any General MIDI bank at all,
    /// including the 215,614,036-byte one and including whatever a developer
    /// dropped in to try — so this is what says the committed file is
    /// [`subset`]'s output rather than a bank somebody found. The exact bytes
    /// are not checkable here: the source bank is not in the repository, and
    /// carrying it to prove a property of its own subset would double what git
    /// holds.
    #[test]
    fn the_shipped_bank_carries_nothing_doom_does_not_select() {
        let want = wanted();
        let sf2 = shipped();

        assert_eq!(
            preset_numbers(&sf2),
            want.melodic.iter().map(|&p| (0, p)).chain([(DRUM_BANK, 0)]).collect(),
            "the shipped bank's presets are not exactly the ones doom selects"
        );
        assert_eq!(
            drum_keys(&sf2),
            want.percussion,
            "the shipped kit does not sound exactly the keys doom plays"
        );
    }

    /// The attribution the licence asks for is inside the file, and what
    /// `licenses/` carries beside it is that same text.
    ///
    /// [`subset`] copies the source's `INFO` chunks rather than writing its own,
    /// so the first half holds by construction — this is what says so out loud
    /// and what fails if a future edit starts composing that chunk instead. The
    /// second half is the one a person can get wrong: `NOTICE` points at a file
    /// in `licenses/`, and nothing but this stops that file drifting from the
    /// terms the bytes we ship actually came under.
    #[test]
    fn the_shipped_bank_still_carries_its_source_licence() {
        let sf2 = shipped();
        let copyright = info_text(&sf2, b"ICOP").expect("the bank has no ICOP chunk");
        assert!(
            copyright.contains("S. Christian Collins"),
            "ICOP is {copyright:?}, which does not name the rights holder"
        );
        let comment = info_text(&sf2, b"ICMT").expect("the bank has no ICMT chunk");
        assert!(
            comment.contains("License v2.0")
                && comment.contains("cannot be 100% sure where all of the samples originated"),
            "ICMT no longer carries GeneralUser GS's licence text and its provenance caveat"
        );
        let beside = std::fs::read_to_string(root().join(LICENCE_PATH))
            .unwrap_or_else(|e| panic!("{LICENCE_PATH}: {e}"));
        assert_eq!(comment, beside, "{LICENCE_PATH} is not the licence inside the bank");
    }

    /// The instrument list is read, so this is what says it was read correctly.
    ///
    /// Fixed numbers because they are a property of a committed file:
    /// `assets/DOOM1.WAD` is byte-for-byte the shareware IWAD (`NOTICE`) and
    /// cannot change. A decoder that lost the percussion split, or read the
    /// count from the wrong offset, still produces a plausible-looking set.
    #[test]
    fn doom1_wads_music_selects_the_instruments_it_selects() {
        let want = wanted();
        assert_eq!(want.melodic.len(), 37);
        assert_eq!(
            want.melodic.iter().copied().collect::<Vec<u16>>(),
            vec![
                0, 6, 7, 10, 11, 15, 18, 29, 30, 31, 32, 33, 34, 37, 38, 40, 41, 42, 44, 45, 46,
                47, 48, 51, 52, 62, 63, 81, 82, 92, 94, 102, 108, 117, 118, 119, 120
            ]
        );
        assert_eq!(want.percussion.len(), 23);
        assert_eq!(
            want.percussion.iter().copied().collect::<Vec<u8>>(),
            vec![
                35, 36, 38, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 55, 57, 59,
                75, 80, 81
            ]
        );
    }
}
