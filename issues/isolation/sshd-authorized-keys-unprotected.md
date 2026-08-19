---
status: open
kind: defect
opened: 2026-08-06
---

# Sshd's keys are as protected as any other file, which is not at all

`/home/root/.ssh/host_ed25519` is the machine's SSH private key and
`/home/root/.ssh/authorized_keys` is the list of who may log in. There is no
user model and no file permissions, so **any process on the machine can read
the first and rewrite the second** — the second being the one that matters:
appending a line to it is a remote login, and nothing stops a process doing it.

This is not an sshd defect and cannot be fixed inside sshd. It is the absence of
the capability-handle model — an owner for a kernel object, and a process that
holds fewer rights than the machine.
Deliberately not worked around here: a daemon-private hiding place would be
obfuscation, and inventing a user model to serve one daemon is the wrong shape
for the decision. Until there is one, **sshd's trust boundary is the machine,
not the account** — anyone who can run code on it can already be anyone.

The daemon does what it can from where it stands: it is not in any boot config,
it offers public keys only, an `authorized_keys` entry carrying options
authorizes nothing (the options are the restrictions, and honouring the key
without them grants more than the file says), and a host key that exists but
does not parse is refused rather than replaced, because minting over it would
change the identity every client has pinned.
