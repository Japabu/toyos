---
status: open
kind: defect
opened: 2026-08-08
---

# `dosfstools`, which was refused, is installed by a committed workflow

`.github/workflows/probe-toolchain.yml:39` installs it beside `qemu-system-x86`
and `ovmf`. It only runs on `ci/probe-toolchain` pushes, so it is not on any
path `main` takes — but it is committed, and a refused dependency left in the
tree is how it comes back. `specs/assessments/ci-plan-assessment-2026-08.md`
§6.1 discusses `fsck.vfat` as an option and does not record that the answer
was no.
