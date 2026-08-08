---
status: open
kind: defect
opened: 2026-08-01
---

# `walk` cannot see an empty directory

`Fat32::walk` returns files only, with directories implied by the `/` in a
path — which is exactly the convention `vfs::FileSystem::list` expects, and
what `TmpFs` and both bcachefs adapters do. The consequence is the same one
the VFS already has: a directory with nothing in it is invisible through
`list`, and the VFS's `created_dirs` set only covers directories created in
this boot. An empty directory that was already on the ESP will not appear.
`Fat32::read_dir` answers correctly per directory; nothing calls it yet.
