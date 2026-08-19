---
status: open
kind: defect
opened: 2026-08-08
---

# A Ring 3 process that sets RFLAGS.TF floods the log forever and is never killed

`popfq` at CPL 3 sets the trap flag, and every instruction after it raises #DB.
`debug_handler` prints a 25-line `HARDWARE WATCHPOINT HIT` report and
**returns** — it clears DR6 and DR7 on the way out, neither of which is TF, and
`iretq` restores the saved RFLAGS with TF still set. So the next instruction
traps again, forever.

Measured with a throwaway `tf` arm on `fault_gate_child` (three instructions:
`pushfq`, `or qword ptr [rsp], 0x100`, `popfq`): **56 and 58 reports** in the
two five-second boots the harness allows, every one `mode=user`, the child
still running when the guest was killed, and the test red on a timeout. The
rate is low only because each report is 25 lines of serial.

Pre-existing and not introduced by the gates work — vector 1 was one of the six
that always had a gate. Two things are wrong and they are separable: the
handler is a debugging facility that a userland process can summon at will, and
it resumes a fault it has no way to stop. #DB from Ring 3 with no debugger
attached has one correct answer, and it is the one every other fault gets.
