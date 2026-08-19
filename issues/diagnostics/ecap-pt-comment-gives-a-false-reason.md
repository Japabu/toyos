---
status: open
kind: defect
opened: 2026-08-05
---

# QEMU 11.0.3 sets `ECAP.PT`, so `Iova::identity`'s comment gives a false reason for correct behaviour

Measured 2026-08-05 off the `hda_probe` boot, against the unit configuration
recorded earlier on 11.0.2:

```
recorded, QEMU 11.0.2:  cap=0x80d2008c222f06c6 ecap=0x0000000000f00f0a … pt=n
this host,     11.0.3:  cap=0x80d2008c222f0686 ecap=0x0000000000f00f4a … pt=y
```

`ECAP` bit 6 is now set and the kernel's own decode prints `pt=y`. `CAP` moved
too (`…06c6` → `…0686`); which bit that is has not been decoded here, and the
raw words are recorded so the next reader need not take a name for it.

**No behaviour is affected.** The kernel writes an identity-mapped domain
always, and never a passthrough context entry, even on a unit that offers one.
What is now false is the *reason* attached to it: `kernel/src/iommu/mod.rs`'s
`Iova::identity` says "§8.1 measured `ECAP.PT` clear on the only unit anyone
here can boot, **so** §5.7's passthrough context type is unavailable" — a
premise this host contradicts, which leaves a correct decision resting on a
reason that has stopped being true. The argument for identity mapping does not
depend on that measurement, and it is the one to keep.
