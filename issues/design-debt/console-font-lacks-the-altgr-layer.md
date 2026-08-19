---
status: open
kind: finding
opened: 2026-08-03
---

# The console font cannot draw most of the Swiss German AltGr layer

`src/assets.rs`'s `console_font` rasterises U+0000..=U+00FF plus box-drawing and
block elements, and `font::draw_char` substitutes `?` for anything else. The
`swiss-german` table is faithful to xkeyboard-config's `ch(de)`, which reaches
well past Latin-1: `€`, `⅛`, `œ`/`Œ`, `ŋ`, `ħ`, `ł`, `ŧ`, `đ`, `ĸ`, `ſ`, `ẞ`,
`Ω`, the arrows on `i`/`u`, and the typographic quotes on `b`/`n`/`v` all render
as `?` on the panel. So do most dead-key compositions outside Latin-1 — `ĉ`, `ń`,
`ẑ`, `Ÿ` and the superscripts — while `â ä à é ç ·` and the rest of Latin-1 are
fine. The bytes delivered to the application are correct in every case; only the
glyph is missing. Widening the rasterised set is the fix; it is a build-time
list, not a code change. `legends_are_renderable` in
`toyos-keymap/tests/detect.rs` keeps the wizard's own prompts inside the covered
range, and it is the only thing that does.
