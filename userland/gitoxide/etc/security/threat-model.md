# Threat Model for Gitoxide - *Provisional*

This document outlines the current understanding of the threat model for the Gitoxide project.

**Note on Scope:** This document is a work in progress and currently provisional in nature. While it will be updated as we learn and refine our processes, a more comprehensive threat model awaits a deeper, component-by-component analysis of all crates and their features.

## 1. Core Security Philosophy & Assets

The primary goal of the Gitoxide project is to provide a safe, correct, and high-performance implementation of Git, in Rust. Our security posture is built on the assumption that we must safely handle untrusted data from multiple sources.

The key assets we aim to protect are:

- **Integrity and Confidentiality of the Host System:** Preventing Gitoxide from being used as a vector to execute arbitrary code, or to read or write files outside of intended directories.
- **Availability of the Host Application:** Ensuring that processing malicious data does not cause the application using Gitoxide to crash, hang, or suffer from resource exhaustion.
- **Integrity of Git Operations:** Ensuring all operations are correct and that an attacker cannot corrupt the repository state in a way that violates Git's security model (e.g., via hash collisions).
- **The Trust of our Users:** The trust of our users is built on designing and implementing Gitoxide as robustly as we can, continuously improving it, and on a commitment to transparency when issues are found and fixed.

## 2. The Gitoxide Threat Landscape

The security considerations for Gitoxide are similar to those of Git itself, and [the SECURITY section of the git(1) manual page](https://git-scm.com/docs/git#_security) is a key reference. However, there are important distinctions arising from Gitoxide's nature as a library. The following sections detail the core principles and specific areas of concern that shape our threat model.

### 2.1. Data Trust Boundaries

#### 2.1.1. Untrusted Remote Repositories and Servers

Remote repositories and the servers that host them are generally treated as untrusted. We assume they may serve malicious, malformed, or unexpected data.

- **Sanitization:** Gitoxide must sanitize all data from remotes. We never automatically install or run hooks provided by a repository we clone. We must also protect against directory traversal attacks during checkout, which includes handling sensitive tree entry filenames (e.g., `..`, `.git`), and malformed or platform-unsupported filenames containing separators or prohibited characters (e.g., `a/../b`, `a\..\b`, `C:x`). Similarly, ref names must be validated to conform to Git's naming rules, and on Windows, they must be prohibited from having reserved names (e.g., `COM1`).
- **Protocol-Level Attacks:** The server may send malicious data that does not conform to expected protocols (e.g., `git-upload-pack` commands or HTTP responses).
- **Data Transported via Insecure Protocols:** Data transported via protocols that inherently do not guarantee integrity (like `http://` or `git://`) is vulnerable to MITM attacks. While we cannot secure the underlying protocol, we must preserve other security guarantees, such as SHA-1 collision detection.

#### 2.1.2. Untrusted Local Repositories

A local repository on the filesystem is not inherently more trustworthy than a remote one. For example, a user might unpack a malicious repository from an archive. Cloning such a repository (even via the filesystem) is a valid way to sanitize it, as the clone operation itself is designed to be safe. Therefore, any repository used as a *source* for a clone must be treated with the same level of scrutiny as a network remote.

- **"Dubious Ownership":** For "dubiously owned" repos, unless allowlisted in a value of `safe.directory` set in a protected scope, Gitoxide must operate in a restricted mode. Our model differs slightly from Git: we will read the repository's `.git/config` file but treat its contents as **untrusted**, refusing to execute any commands or perform other dangerous actions based on its configuration. This allows for broader library use cases without requiring users to unsafely take ownership of untrusted files.

#### 2.1.3. Untrusted Environment & Filesystem Locations

The environment in which Gitoxide runs is not fully trusted.

- **Working Tree:** The contents of a repository's working tree are untrusted, as they are derived from (untrusted) repository history.
- **Current Working Directory (CWD):** The CWD is an untrusted search path for executing external programs. We do not execute programs from the CWD unless their path explicitly indicates a local execution (e.g., prefixed with `./`).
- **Application Directory (on Windows):** In some scenarios (e.g., an installer in the `Downloads` directory), the directory containing the executable is also an untrusted search path. This poses a risk when invoking subprocesses (e.g., `git`), as a malicious executable with the same name could be found and run from that directory.

### 2.2. Trusted Data and Responsibilities

- **The `.git` Directory:** In a trusted repository (i.e., one that passes ownership checks), files within the `.git` directory (like `config` and `hooks`) are considered trusted. Our responsibility is to ensure that untrusted data can never tamper with the contents of this directory.

## 3. Formal Threat Analysis (STRIDE Summary)

The following table summarizes the primary threats to Gitoxide using the STRIDE framework. The "Details" column references the relevant section in the narrative landscape above.

| Interaction / Component | Threat & Summary | STRIDE Category | Details |
| :--- | :--- | :--- | :--- |
| **Cloning/Fetching an Untrusted Repository** | A crafted repository causes writes outside the working tree. | **T**ampering, **E**levation of Privilege | 2.1.1 |
| | A malformed packfile or "git bomb" exhausts memory/CPU. | **D**enial of Service | 2.1.1 |
| | An object with a colliding SHA-1 hash is injected into the repo. | **S**poofing, **T**ampering | 2.1.1 |
| **Reading Local Repository Configuration** | A malicious `.git/config` in a "dubiously-owned" repo executes code. | **E**levation of Privilege | 2.1.2 |
| | A malformed `.git/config` file causes the library to panic. | **D**enial of Service | 2.1.2 |
| **Invoking External Processes (`git`, shells)** | A malicious executable (`git`, `sh`) is found first in an untrusted search path. | **S**poofing, **E**levation of Privilege | 2.1.3 |
| | A malicious external process hangs, causing the host app to hang. | **D**enial of Service | 2.1.3 |
| **File Checkout** | A file path in the index targets a reserved device name on Windows. | **D**enial of Service | 2.1.1 |
| | A file path uses case-folding or equivalent names to overwrite another file. | **T**ampering | 2.1.1 |

## 4. Mitigation Strategies

This section outlines our primary countermeasures for the threats identified above.

| Threat Category | Mitigation Strategy |
| :--- | :--- |
| **Filesystem Tampering & EoP (Traversal, Special Filenames)** | - Rigorous path sanitization before all filesystem writes. <br> - Block traversal (`../`), git-dir writes (`.git/`), and special Windows device names. <br> - Prohibit patterns that can alias sensitive directories via OS-specific equivalences (e.g., case-folding, 8.3 names, NTFS streams, HFS+ ignorable characters). |
| **EoP via Malicious Local Configuration** | - Implement ownership checks on local repositories. <br> - For "dubiously owned" repos, unless allowlisted, treat all configuration values as untrusted and never execute commands based on them. |
| **EoP via Spoofed External Processes** | - Use secure, well-defined search paths when invoking external commands. Do not execute programs from the CWD unless explicitly requested (e.g., `./program`). <br> - *Being Investigated:* The risks of using `std::process::Command` in scenarios like installers on Windows. |
| **SHA-1 Collision Attacks** | - Implement detection for known SHA-1 collision methods. <br> - We hope to support SHA-256 repositories in the near future. |
| **Denial of Service (Resource Exhaustion)** | - Write panic-safe parsing logic for all Git data structures. <br> - Apply sensible resource limits during resource-intensive operations like packfile decompression. |
