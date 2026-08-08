---
status: open
kind: finding
opened: 2026-08-06
---

# Nothing connects to sshd, so its accept path is read-verified

`tests/sshdcase` boots sshd with a NIC and certifies the half that needs a
machine: that it mints an identity under `/home`, that it names the file it
authenticates against, and that with no usable key it exits instead of holding
port 22. The decision itself — this key yes, that key no, an options line
never — is host-tested in `userland/sshd`'s own `#[cfg(test)]` module against
real Ed25519 keys and `ssh-key`'s parser.

What neither reaches is a client. No test completes an SSH handshake, so the
wiring between russh's auth callbacks and that decision — `auth_publickey`,
`auth_publickey_offered`, and the `MethodSet` that stops password auth being
offered at all — is certified by reading. Closing it needs an SSH client on the
host talking to the guest through `hostfwd`, which is
`specs/daemon-testability.md` §6's step 1 and belongs with gate N.
