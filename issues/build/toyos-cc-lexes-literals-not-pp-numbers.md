---
status: open
kind: finding
opened: 2026-08-09
---

# The preprocessor lexes numeric literals, not C99 pp-numbers

C99 6.4.8 makes a *preprocessing number* one token: a digit or `.digit`
followed by any run of digits, identifier characters, `.`, and `e±`/`E±`/`p±`
/`P±`. `toyos-cc`'s `tokenize_pp` instead recognises the numeric literal shapes
it knows — hex, binary, decimal, scientific, hex-float and a suffix list — so a
pp-number it does not recognise splits into two tokens. Measured with
`toyos-cc -E -P`:

```
1e+5        1e+5        one token
0x1p-3      0x1p-3      one token
1.0f        1.0f        one token
0xFFUL      0xFFUL      one token
1u          1u          one token
9999b       9999 b      split
CAT(12,ab)  12 ab       split
123defg     123 defg    split
```

**Narrow, and currently unobservable.** It can only surface where the
preprocessor's own text is the product rather than input to the lexer behind
it, which for this compiler is a `.S` file it never assembles. `pp_tcc/12.S`
is the one case in the corpus that exercises it (`.long 9999b, 6001f`), and
that file now stops earlier, on GNU's named variadic parameter, which is
refused by name.

Taking it would be in charter — a pp-number is conforming C99 and this is a bug
in something already implemented. It is not taken because the change is to how
*every* number lexes, `preprocess/consteval.rs` reads those same tokens to
evaluate `#if`, and there is nothing in the tree today that can tell whether
the new rule is right. Declared here rather than left as a third silent year.
