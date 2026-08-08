# Doom's music: what it needs, and what each way of supplying it costs

Status: **options priced, decision open.** The licence call is the owner's.

`b8b0749` removed `assets/timgm6mb.sf2` because it is GPL-2.0 under an
MIT OR Apache-2.0 tree, and made music opt-in from `assets/soundfont.sf2`. That
fixed the licence and left the milestone unmet. This is the survey that decides
what goes back in.

Every number below came from a command run on 2026-08-08. Sizes are bytes as
reported by `curl -I` or `stat`; MiB conversions are 1048576.

## 1. What Doom's music actually needs

`assets/DOOM1.WAD` (4,196,020 B) holds **13 MUS lumps** totalling 245,179 B. A
MUS header declares its own instrument list, so the requirement is enumerable
rather than estimated — read straight out of the WAD:

| | count |
|---|---|
| melodic GM programs referenced | **37** of 128 |
| melodic programs never referenced | **91** |
| GM percussion keys referenced | **23** (35–53, 55, 57, 59, 75, 80, 81) |

The 37: 0, 6, 7, 10, 11, 15, 18, 29, 30, 31, 32, 33, 34, 37, 38, 40, 41, 42, 44,
45, 46, 47, 48, 51, 52, 62, 63, 81, 82, 92, 94, 102, 108, 117, 118, 119, 120.

**Doom needs 29% of a General MIDI bank.** That is what makes option 2 real.

The playback path is `MUS lump → mus2mid (C, doomgeneric) → rustysynth::MidiFile
→ Synthesizer(SF2)`. `userland/doom/src/sound.rs:514` loads
`/share/soundfont.sf2`. Any valid `.sf2` is a drop-in; nothing else changes.

## 2. Constraints that eliminate candidates before size

- **rustysynth 1.3.6 cannot read `.sf3`.** Not a gap to work around — it is an
  explicit refusal: `SoundFontError::UnsupportedSampleFormat`, "SoundFont3 is
  not yet supported" (`soundfont_sampledata.rs:58`). `.sf3` is Vorbis-compressed
  and a Vorbis decoder is a new dependency, which the Dependencies rule forbids.
  **Every `.sf3` is out, whatever its licence.**
- **The old font was fetched from an aggregator.** The deleted `build.rs` pulled
  TimGM6mb from `github.com/craffel/pretty-midi` — a Python MIDI library's repo,
  not Tim Brechbill's. It was GPL-2.0 *and* had no chain to its rights holder.
  Both halves of that mistake are worth not repeating.
- **PCM does not compress.** `gzip -9` on the winning subset saves 8%
  (15,546,748 → 14,287,270). There is no packaging trick that changes the shape
  of this decision.

## 3. The candidates, measured

Baseline: the removed **TimGM6mb is 5,994,284 B**, GPL-2.0. The image today is
131,072,000 B and `image.rs` sizes it from content (`round_up_sectors`), so
there is no cap to overflow — a soundfont costs its own bytes and no more.

| candidate | bytes | licence | verdict |
|---|---|---|---|
| MuseScore_General.sf2 | 215,614,036 | **MIT** | too big whole |
| MuseScore_General.sf3 | 39,900,972 | MIT | **unreadable** — rustysynth refuses sf3 |
| GeneralUser-GS.sf2 v2.0.3 | 32,319,396 | bespoke "License v2.0" | too big whole; licence needs a call |
| FluidR3 (fluid-soundfont.tar.gz) | 130,294,103 | MIT | too big |
| FreePats General MIDI | — | **GPL-3.0** | same defect as TimGM6mb |

### The licence facts, from the rights holders

**MuseScore_General — MIT, with the strongest provenance available.** The
licence file names its chain: Frank Wen (Fluid, 2000–2008), Michael Cowgill
(Mono conversion, 2014–16), S. Christian Collins (MuseScore adaptation,
2018–19). It ships `MuseScore_General_Sample_Sources.csv` (12,817 B) crediting
each preset's samples *and their original licence* — e.g. the grand piano reads
"Original License: Public Domain (confirmed via AKAI rep.)". Condition:
copyright notices must survive into derivatives.

**GeneralUser GS — not CC-BY-4.0.** This corrects the record: `system.toml`
currently says CC-BY-4.0 and that is wrong. v2.0.3 carries a bespoke
"License v2.0" by S. Christian Collins. It is permissive in substance — "use
without restriction… private or commercial", "feel free to use it in your
software projects, and to modify the SoundFont bank or its packaging" — but it
is not a standard licence, has no explicit patent or warranty clause, and states
plainly:

> Because GeneralUser GS originated as a personal project with no intention for
> publication, I cannot be 100% sure where all of the samples originated,
> although I do know that none of them came from commercially published
> SoundFont packages or sample CDs.

It also asks that others not hotlink its download files. That is a request about
distribution, not a restriction on redistribution, but it belongs in the
decision.

## 4. Subsetting works, and it is provably free

A subsetter (`sf2sub`, 360 lines, scratch, no dependency) keeps only the 37
melodic presets plus the bank-128 drum kit, prunes drum zones by key range,
rebuilds every `pdta` table and the sample pool, and carries the source's `INFO`
chunks — so `ICOP`/`ICMT` survive and the attribution condition is met by
construction rather than by remembering.

| source | whole | Doom subset | presets / samples kept |
|---|---|---|---|
| GeneralUser GS | 32,319,396 | **15,546,748** (14.83 MiB) | 38 of 287 / 358 of 920 |
| MuseScore_General | 215,614,036 | 144,022,862 (137.35 MiB) | 38 of 309 / 514 of 1246 |

**The subset is not an approximation.** Rendering each of the 13 tracks at 44.1
kHz through the guest's own path — the real `mus2mid.c` and rustysynth 1.3.6 —
the subset output is **bit-exact against the full bank** for all 13, verified by
`cmp`. Subsetting costs exactly zero audible quality; it only deletes presets
Doom never selects.

**MIT-and-small is not reachable.** MuseScore's subset is dominated by one
preset: **Acoustic Grand Piano alone is 100,403,474 B** (70% of the subset), and
Doom uses it only in `D_INTRO` (1,485 B) and `D_INTROA` (631 B). Dropping it
still leaves 43,620,774 B (41.60 MiB) — three times GeneralUser's whole subset.
Halving the sample rate would still land near 21 MiB, costs quality, and needs
resampling code. MuseScore is high-fidelity by design and that is the whole
problem.

## 5. The zero-byte option nobody listed: OPL3 + GENMIDI

`DOOM1.WAD` already contains **`GENMIDI`, 11,908 B, header `#OPL_II#`** — the
FM instrument bank Doom shipped with: 175 records (128 melodic + 47 percussion),
36-byte patch + 32-byte name each, parsed and confirmed. This is the sound
players actually remember, and **the data is already in the image**.

- Size: **0 added bytes.**
- Licence: **no new question at all** — it is data in a WAD we already ship.
- Cost: an OPL3 (YMF262) core plus a GENMIDI voice allocator. The core is 18
  2-operator channels — phase generator, envelope generator, waveform select,
  KSL/KSR, feedback — written from the datasheet. Honestly **3–6 days**, and
  the reference implementations are LGPL (Nuked-OPL3), so they can be consulted
  for behaviour but not transliterated.
- Risk: **this is the option whose quality cannot be asserted in advance.** An
  approximate OPL3 sounds wrong in a way listeners notice immediately, and there
  is no bit-exact oracle to check against the way §4 had one.

## 6. Options, ranked

**1. GeneralUser GS subset — 15,546,748 B (14.83 MiB).** Bit-exact for all 13
tracks. Needs the subsetter (~360 lines, ours) run once at image build, or its
output dropped into `assets/`. 2.6× the GPL font it replaces, 11.9% on top of
today's image. **Blocker: the licence is bespoke and the provenance is soft.**
That is a judgment about redistribution risk and belongs to the owner, not to an
agent. Everything technical about this option is settled and green.

**2. OPL3 + GENMIDI — 0 bytes.** Best on licence and size by a distance, and the
most authentic result if it is done well. Costs days of work and carries the
only real quality risk in this list. The right answer if image size is the
binding constraint, or if the licence answer to option 1 is no.

**3. MuseScore_General subset — 144,022,862 B.** The licence answer everyone
wants (MIT, named chain, per-sample provenance) at a size nothing can justify.
Listed because it is the only *clean* licence available, and because 41.60 MiB
without the piano shows how little headroom trimming buys.

**4. Generate a bank from scratch.** The `src/wallpaper.rs` precedent does not
carry: a gradient has no wrong answer and a distortion guitar does. Doom's set
is 37 programs weighted toward overdriven/distortion guitar, bass, strings and a
23-key drum kit — the hardest things to synthesise convincingly. Strictly worse
than option 5, which gets authentic FM timbres from data we already ship.
**Not recommended.**

**5. Declare the milestone met with music opt-in.** Rejected, and recorded so it
is visibly rejected: the owner has called a soundfont a milestone blocker, and a
default-silent Doom does not meet it.

## 7. What is not verified

- **No guest boot was taken.** The renders in §4 use the guest's own `mus2mid.c`
  and the same rustysynth 1.3.6, so the soundfont decision is fully covered; what
  is untested is only the audio pipeline underneath, which is orthogonal to which
  `.sf2` is loaded and is what gate A already measures.
- `sf2sub` is a scratch tool, not in the tree. If option 1 is chosen it needs a
  home, a test, and a decision about whether the image build subsets at build
  time or ships a pre-made file.
- Nothing here was committed to `assets/`; `.gitignore:7` (`assets/*.sf2`) means
  a dropped-in font cannot be committed by accident.
