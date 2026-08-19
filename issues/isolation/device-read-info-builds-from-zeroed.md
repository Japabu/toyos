---
status: open
kind: defect
opened: 2026-08-08
---

# `device::read_info` builds its `T` with `mem::zeroed` and fills it from a read

Filed out of the SDK IPC-framing entry when that closed.

`device::read_info<T: Copy>` (`toyos/src/device.rs:10`) is the same shape
`recv_payload` had before it was bounded: a `T` conjured with `mem::zeroed` and
then overwritten from a read whose length nothing ties to `size_of::<T>()`.
Lower stakes, because the bytes come from the kernel rather than from a peer —
which is the only reason it is a defect and not the same defect.

`IpcPayload` / `ipc_payload!` is the bound it wants.
