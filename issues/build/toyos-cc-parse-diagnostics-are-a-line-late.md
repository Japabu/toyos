---
status: open
kind: defect
opened: 2026-08-09
---

# Every parse-time diagnostic names the line after the one it is about

`Parser::loc` reports a line one greater than the token's. Measured:

```
$ printf 'int b __attribute__((weak));\n' > loc2.c
$ toyos-cc -c loc2.c
loc2.c:2:22: __attribute__((weak)) is not implemented by toyos-cc...
```

The construct is on line 1, column 22, and the column is right. It is one late
for every parse-time refusal, so it is in the lexer's line counting or in the
`#line` bookkeeping the preprocessor emits, not in any one diagnostic.

Pre-existing, and visible in the corpus long before it was noticed:
`03_struct.c` refuses its `__attribute__((__cleanup__))` at "11:23" and the
attribute is on line 10.

Two things make this worth fixing rather than living with. Refusing by name is
the compiler's whole answer to a construct it does not implement, and a name
plus the wrong line is a worse answer than it looks. And `NOT_RUN`
(`tests/toyos.rs`) now quotes those refusals, so anything that shifts the
numbering shifts a fixture with it.
