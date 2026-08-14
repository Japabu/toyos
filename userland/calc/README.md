# calc

A calculator with two layouts: Calc, which works in exact rationals and falls
back to 40-significant-digit decimals only where the value is irrational, and
Prog, which is 64-bit two's-complement integers shown in hex, decimal and binary
at once.

The arithmetic is this crate's own — big integers, rationals over them, and the
series that produce the approximations. Everything except `src/main.rs` is
UI-independent and tested on the host.

Depends on the font library, winit and softbuffer.
