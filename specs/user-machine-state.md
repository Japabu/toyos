# The user machine state

> A transition out of Ring 3 that can reach another task must save and
> restore **the whole** user machine state, and a task that has never been in
> Ring 3 must start from a **declared** state.

## 1. The state

The user machine state on this machine is exactly:

- the general-purpose registers;
- `RIP`, `RSP`, `RFLAGS`, `CS`, `SS`;
- `FS.base`;
- XMM0–15 and `MXCSR`;
- the full x87 state: registers, control, status and tag words, opcode, and
  the instruction and data pointers.

`XCR0` is 1 — x87 and SSE, nothing else — on every CPU: no CPU sets
`CR4.OSXSAVE`, every CPU's FP configuration is checked at boot, and AVX
instructions `#UD`. Kernel code executes no floating-point instructions, so
nothing disturbs a task's FP state between its save and its restore.

## 2. Rules

1. **Every transition out of Ring 3 that can reach another task saves the
   full FP state, and restores it as the transition's last act before
   returning to Ring 3** — after any point at which the task could have been
   switched. A transition that cannot reach another task before returning is
   exempt; every other one is not.
2. **The save must not raise a pending exception.** A task may enter the
   kernel with an unmasked x87 exception pending; the save uses the
   non-waiting forms, and the pending exception reaches only the task that
   caused it.
3. **A task that has never run in Ring 3 starts from the declared state:**
   `FCW = 0x037F`, `FTW = 0`, `MXCSR = 0x1F80`, every other field and
   register zero. The image is loaded whole; the hardware's init instruction
   does not produce it.
4. **The save is `FXSAVE64`/`FXRSTOR64`, unconditionally** — the 64-bit
   forms, preserving the full instruction and data pointers. Every machine
   this kernel runs on executes the same save, and on this machine it is a
   complete save of everything `XCR0` permits to exist.
5. **`XCR0` never names a component the save does not cover.** Enabling any
   component beyond x87+SSE — AVX-512 is the standing candidate — requires
   the save to become XSAVE at the same moment; `FXSAVE64` over a wider
   `XCR0` is a silent partial save and never ships.
6. **Vector 0x07 (`#NM`) kills the faulting process.** No kernel mechanism
   defers work behind that vector.

## 3. Exclusions

- **Lazy FP restore** — it defers the restore behind `#NM`, leaking the
  previous task's register file speculatively across the deferral boundary.
- **An XSAVE path or any capability branch in the save** (§2.4): one save,
  every machine.
