---
status: open
kind: defect
opened: 2026-08-03
---

# A tmpfs backing serves zeros after its file is deleted instead of saying the file is gone

Unlink a file while a process is still demand-paging it and the backing keeps
serving reads.


`file_backing::FileBlocks` is now the one extent list every backing for a name
reads through, and `BcacheFsAdapter` revokes it wherever the filesystem hands
the blocks back — `delete`, `delete_prefix`, a `rename` over an existing
destination, and the truncating `create`/`create_symlink`. It is keyed by name
rather than `FileId` because `open_backing` — the one a running program's text
lives behind — never opens a file. A read after revocation is `Err`, which the
fault handler already leaves unhandled and `file_cache::read_page` zero-fills.

**Revocation, not lifetime extension**, and deliberately not the capability
refcounting this entry used to ask for: keeping a deleted file's blocks alive
for as long as something can read them is POSIX's answer to a question ToyOS
has not been asked, and doing it honestly still needs
`specs/capability-handles-spec.md`. Making the stale read *fail* is the whole of
what the disclosure needs and it is expressible today.

Gate: `home_backing_revoked`, in the shared boot. It asserts the read is
**zeros**, not merely "not the attacker's byte", so it cannot pass by the blocks
failing to be reused.

**`/tmp` is still open**, and is a correctness wart rather than a disclosure:
`TmpfsBacking::read_page` (`tmpfs.rs:25`) reads through `file_cache` by
`FileId`, and a dropped file's `copy_page_out` fills zeros, so the process
faults in blank pages instead of being told the file is gone. Nothing is
disclosed — tmpfs has no allocator handing the storage to anybody else — so it
wants the same revocation only for honesty, not for safety.
