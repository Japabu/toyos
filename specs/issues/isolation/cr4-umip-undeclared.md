---
status: open
kind: finding
opened: 2026-08-09
---

# `CR4.UMIP` is free Ring 3 hardening that nobody has declined


`arch/control_regs.rs`'s `CR4_OPTIONAL` is `SMEP | SMAP | PCIDE`. `UMIP` (bit
11) is in neither it nor `CR4_REQUIRED`, so it is clear on every CPU — and since
`write_cr4` writes the register whole, that is a decision this code takes on
every boot rather than a default it inherited. With it set, `SGDT`, `SIDT`,
`SLDT`, `SMSW` and `STR` executed in Ring 3 raise `#GP` instead of handing a
process the GDT, IDT and TSS addresses (SDM Vol. 3A §2.5). Those addresses are
what a KASLR bypass is built out of; nothing in this kernel's userland executes
any of the five, and CPUID reports the feature at
`CPUID.(EAX=07H,ECX=0):ECX[2]`, the same shape as the three optional bits
already there.

Raised in PR #7's adversarial review as the notable omission from a declaration
that decides every other bit. Not taken in that branch, because adding a bit is
a behaviour change and the branch's job was the declaration itself. Adding it
now costs a line in `control_regs`' gate as well (`tests/toyos.rs`), whose new
rule is that a bit named nowhere may not be set — which is the rule working, not
an obstacle.

The same review asked what `CR4.DE` is for, since this kernel programs no debug
register. That one is answered in place: `CR4_REQUIRED`'s doc now says it is
zero-legacy — `DE` clear is the 386 behaviour where `DR4`/`DR5` alias `DR6`/`DR7`
— and not a dependency of anything here.

---

## 2. The panic path

