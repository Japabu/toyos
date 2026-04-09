# Threat modeling notes

*These are fragmentary thoughts about our threat model. They are currently incomplete in many ways, but especially in that they do not examine the ecosystem of software that uses gitoxide.*

## Similar, but not identical, to Git security considerations

The security considerations and threat landscape for Gitoxide is similar to that of Git, with several of the most important differences being:

1. Gitoxide is primarily a library project. Used as a library, it contains a the `gix` crate (which most users declare as their dependency) and numerous more special-purpose `gix-*` crates.

2. Gitoxide does not ship a Unix-like environment on Windows. We prefer instead to treat Windows as a "first-class" platform, and because we are primarily a library, there is no clear reasonable way to ship an MSYS2 or similar environment of our own. However, typical operations on Git repositories often include running shell scripts and other commands that expect Unix-like tools and some aspects of Unix-like path semantics. We try to accommodate this on Windows, and we also search for a suitable POSIX-compatible shell to run shell scripts in, preferring one that accompanies a Git for Windows installation when it is present and when we can find it. The uncertainty associated with the environment in which users may configure some custom commands to run makes it so that some assumptions that might seem safe for us to make are not.

3. We do not carry our own installation-scoped configuration. Instead, we use the one that Git provides, when present. The Git installation scoped configuration is usually the `system` scope, but some `git` builds (specifically, Apple Git on macOS) have a higher `unknown` scope. When not set or suppressed by an environment variable, the `system` scope configuration file, if present, is usually `/etc/gitconfig` except on Windows, but that is not guaranteed. We do not require that `git` be installed, but if it is then we want to respect the values of any variables in its installation-level configuration scope except where overridden in a narrower scope. To do this, we attempt to invoke `git` to ascertain the appropriate path. We have to make sure the program we're running is `git` and not an attacker-controlled decoy, and that we parse the output correctly even on systems with unexpected configurations.

4. We model trust for local repositories differently: while `git` refuses to read the configuration file at all when a repository has "dubious ownership," we will read the configuration file but report variables from it as untrusted to the caller, and always refrain from performing actions such as running commands based on them. One reason for this difference is to allow broader use as a library, while avoiding a scenario where a user or application would dangerously mark untrusted local files or directories as safe (by taking ownership of them or listing them as a value of `safe.directory`) in order to read a configuration without attempting to follow it.

Aside from the subtle differences in point (4) above (and also that we do not yet have our own implementation of `upload-pack`), [the SECURITY section of the git(1) manual page](https://git-scm.com/docs/git#_security) applies fully.

The rest of these notes are, therefore, not unique to Gitoxide, though they are presented in the context of Gitoxide, and they emphasize areas we have found we need to be especially careful about.

## What data do we trust?

### Users should be able to safely clone untrusted repositories

Remote repositories that we clone or otherwise fetch from are untrusted. Although there are numerous exceptions to this in practice--where users know they are cloning a repository they control fully--we never actually know that this is the case and we rarely if ever would benefit from knowing:

- We always treat remote repositories as untrusted and we never install or run hooks based on any valid or malformed configuration or other content in them.

- We always check that files we would create in a checkout, whether the checkout is done as part of a clone or subsequently, cannot carry out a directory traversal attack. We need to prevent upward traversal, where cloning a repository would create files outside of the directory where the repository working tree would be checked out. We also need to prevent downward traversal, where cloning a repository would create files in "holes" in the repository that are not considered part of the working tree and that may be sensitive, such as the repository's own `.git` dir, the working trees of submodules, and the `.git` dirs of submodules. Preventing directory traversal involves some checks that always apply and others that apply on particular operating systems or filesystems, related to case folding, other forms of equivalence, NTFS alternate data streams, Windows 8.3 short names, and what characters are directory separators (in particular, both `/` and `\` are directory separators on Windows, while on Unix-like systems a tree or blob can be checked out to a location with `\` in its name).

- On Windows, we always check that files we would create in a fetch (including refs) and checkout do not have names that are treated as legacy DOS devices on any Windows systems (e.g., `COM1`, `CON`, `CON.txt`, `CONIN$`, and numerous others), at least for devices that can in practice exist (i.e., it is probably okay to fail to block the COMn and LPTn where n is a Unicode superscript, since those are distinct from the ones where it is an actual digit and the superscript device names never in practice actually exist; other than that, everything that is a reserved name, plus `CONIN$` and `CONOUT$` which behave as reserved names even though technically they are not, must be blocked.

- We always check that refs have valid names according to the Git rules for how a ref can be named, before performing operations based on them that have known security implications. This especially includes the operation of creating a loose ref for it in the object database.

Remote repositories cannot be trusted to pass any `git fsck` or other validation checks or otherwise to satisfy the technical requirements of "being a Git repository."

### Users should be able to clone from untrusted servers

The servers that host remote repositories cloned via network transport must be assumed untrusted as well:

- The server may send us specially crafted malicious data that do not conform to expected protocols. For HTTP this includes the possibility of an attacker-controlled web server.

- More straightforwardly, the server may have malicious (or even just malfunctioning) implementations of `git-*-pack` commands that are used when cloning.

- The exception is that we cannot protect users who trust a malicious server in ways that rely on the server preserving integrity of data that pass through it. Specifically, if a user pushes to a server and relies on that server providing the same data back, and then fetches from it elsewhere, then we cannot protect the user from that loss of integrity.

### Transport over untrusted networks should be as secure as possible

The network over which transport occurs must not be trusted unless a protocol is being used that inherently trusts the network:

- SSH, and HTTP with SSL/TLS (`https://`), must ensure authenticity, unless the user has explicitly allowed connections to proceed otherwise.

- HTTP without SSL/TLS (http), to the extent to which it is permitted at all, cannot ensure authenticity, nor can the Git protocol (`git://`).

- However, even if the user explicitly chooses to use a protocol that is vulnerable to man-in-the-middle attacks, we still need to preserve authenticity expectations related to other functionality, e.g., ensuring that SHA-1 OIDs are processed with collision detection (against collisions produced in the known feasible ways of doing so) and working toward supporting repositories with SHA256 OIDs.

### Users should be able to sanitize by cloning via the filesystem

One way to neutralize a potentially malicious configuration in a locally present repository, such as a repository that was downloaded and unpacked from a .tar archive or made available by another user in share location, is to clone it (leveraging the sanitization performed by `git-upload-pack`), and this is sometimes done via the filesystem. So remote repositories where the remote is the same machine, even if they are cloned through the filesystem rather than via network transport, are *just as untrusted*.

### Working trees and current working directories are unsafe search paths

The current working directory is an untrusted search path in nearly all cases, both because it may be the checked-out working tree of a repository (or branch) whose content on the remote was attacker-controlled and that was faithfully cloned, and more generally because the CWD can be anywhere (e.g., `/tmp`) and need not be trusted.

### Contents in a git-dir are trusted, and that directory must be protected…

Files like a repository's `.git/config` and its `.git/hooks` directory are trusted in ordinary use, and we are responsible for ensuring that nothing untrusted gets in there.

### …But we must only trust it if user-"owned" or allowlisted

Because we trust files in a repository's `.git` dir (or just in the repository directory if it is a bare repository), we must refuse to perform most operations on local repositories whose filesystem metadata (on the relevant files and directories of/in them) do not indicate that the user who owns the process is also the owner of the repository, unless the user has explicitly configured the relevant path(s) as trusted:

- On Unix-like systems, this sense of ownership of a file/directory corresponds to the ownership model supported by the filesystem and operating system, because every filesystem entry on a Unix-like system has a user (or UID) as its owner (with a *separate* group ownership mechanism).

- On Windows, this only partially coincides with the ownership model supported by the filesystem and operating system, because filesystem entries (like securable objects in general) may be owned by any SID (security identifier), not necessarily a user. There are major important situations where the owner is not a user but where usability degrades greatly if we refuse to trust the local repository, so we have various special cases. These are intended to be the same or almost the same as those in Git for Windows, and are intended in any case never to be any less secure.

- As in Git, the `safe.directory` configuration variable must be ignored in any non-protected scopes (local and worktree scopes) but honored in protected scopes as an allowlist of paths that can be trusted as if owned by the current user even when they are not.

### Being investigated: Gitoxide in installers run from from partly untrusted directories

In some use cases, an application's own containing directory is an untrusted search path. On Windows, that may be relevant if gitoxide library crates are used as part of an installer. If the user downloads the installer to their `Downloads` directory, then through our use of `std::process::Command`, subprocesses of the installer will be searched for in that directory, possibly picking up malicious programs that have been downloaded but not examined or previously run.

- This is slightly mitigated by how programs in `Downloads` often have the "mark of the web" alternate data stream data, prompting the user; but the user may interpret the prompt as pertaining to the installer they deliberately just ran and allow it. This is entirely separate from the issue where we must not trust the current working directory--this is about the directory that contains the executable itself.

- We are still evaluating the likelihood and impact of this use case.

- Implementing path search ourselves in more (or all) cases may be a solution, but this carries its own risks that we may make mistakes ourselves that would otherwise be avoided by using `std::process::Command`'s own path search logic for Windows. (We already face this to some extent, in that there are already circumstances where we have to reimplement path search on Windows, in order to find and run files that would not otherwise be found, such as shell scripts with `#!` lines that make them "executable.")

### Being investigated: Can we select CodeQL queries to reflect these subtleties?

In CodeQL, a combination of queries, including all those from "remote only" and a hand-picked selection of those from "remote and local" could be used.

(This is separate from the goal of accurately *stating* in a threat modeling document what the threat model is. But hopefully either one, if done, would help figure out how to do the other.)
