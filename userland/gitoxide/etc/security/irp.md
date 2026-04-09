# Gitoxide Incident Response Plan (IRP) for Vulnerabilities

This document outlines the procedure for responding to security incidents, with a primary focus on the discovery and handling of vulnerabilities in Gitoxide. It is a living document that will be updated as we learn and refine our processes.

The primary goal during any incident is to remain calm and methodical to ensure a thorough and effective response.

This plan supports two disclosure strategies:

- **Issue Advisory With Patch:** The standard and most common path, where a fix is prepared privately and released at the same time the vulnerability is publicly disclosed in an advisory.
- **Issue Advisory Early:** The less common path, where we publish an advisory to disclose the vulnerability publicly *before* a fix is available.

The following steps are written for the standard "Issue Advisory With Patch" path, with notes indicating how the process changes for the "Issue Advisory Early" path.

## Phase One: Initial Triage and Assessment

This phase begins when a potential vulnerability is reported to us. Usually this is either by a member of the GitoxideLabs org itself via a draft GitHub Security Advisory, by anyone by Private Vulnerability Reporting (PVR) on GitHub or by email, as outlined in [`SECURITY.md`](https://github.com/GitoxideLabs/gitoxide/blob/main/SECURITY.md). However, this also applies if a vulnerability is communicated in some other way, such as by being publicly disclosed. (Immediate public disclosure is plausible even if the reporter values coordinated disclosure, because a bug that the reporter believes is not a vulnerability, or whose security implications are unknown to the reporter, might be reported initially in a GitHub issue or other public post.)

1. **Acknowledge the Report**: Aim to provide an initial response to the reporter within 72 hours, acknowledging receipt of the report.

2. **Understand the Report**: Carefully review the report to ensure a full understanding of the claimed vulnerability. If any part is unclear, request clarification from the reporter.

3. **Validate the Vulnerability**:

   - Assess whether the described behavior, if accurate, constitutes a security vulnerability. It must be plausibly exploitable and have a negative impact on confidentiality, integrity, or availability (C/I/A).
   - Confirm that the vulnerability lies within Gitoxide rather than exclusively in third-party code or the surrounding environment, or that the vulnerability arises from a specific way Gitoxide interacts with other software that can be fixed in Gitoxide more feasibly than in other software.
   - If a Proof-of-Concept (PoC) is provided, attempt to reproduce the issue as described. If the PoC fails, investigate further to determine if a vulnerability still exists. Work with the reporter to refine the PoC if necessary. If no PoC is included, write one and test it.

4. **Initiate Advisory**:

   - If the report was submitted by email, [create a new draft GitHub Security Advisory](https://github.com/GitoxideLabs/gitoxide/security/advisories/new) for the vulnerability.
   - Alternatively, we can request that the reporter create the draft advisory via Private Vulnerability Reporting, unless they express a preference for us to manage it.

5. **Triage Severity**: Perform an initial severity assessment. Determine the potential impact on confidentiality, integrity, and availability, and either calculate a [CVSS score](https://www.first.org/cvss/) or validate/adjust the score suggested by the reporter.

6. **Choose Disclosure Strategy**: Based on the assessment, decide which disclosure path to follow. While "Issue Advisory With Patch" is the default, choose "Issue Advisory Early" if it is determined that we should publish an advisory before a fix is ready. Reasons for this include, but are not limited to:

   - The vulnerability is confirmed to be actively exploited in the wild.
   - The vulnerability has already been disclosed publicly, by the original reporter or by another party.
   - The vulnerability is low risk, the fix is expected to be lengthy, and we believe that awareness of the issue would benefit users more than withholding it.

   This step can sometimes be deferred. That is, sometimes we may further investigate or coordinate before deciding to issue an advisory prior to making a fix available.

## Phase Two: Investigation and Coordination

Once a vulnerability is validated, a deeper investigation is required to understand its full scope and impact.

1. **Scope the Impact within Gitoxide**:

   - Identify which specific crate(s) are affected.
   - If possible, identify which versions are affected, or otherwise when the vulnerability was introduced.
   - Determine the use cases or APIs that trigger the vulnerability.
   - Ascertain if the vulnerability is platform-specific (e.g., Windows only, Unix-like only) or affects all operating systems.

2. **Assess Risk and Ecosystem Impact**:

   - Make a rough estimate of the likelihood of exploitation.
   - Assess the potential impact on Gitoxide users and the broader ecosystem of dependent libraries and applications.

3. **Analyze for Broader Implications**:

   - Investigate if the vulnerability is similar to previously discovered issues in Gitoxide. If so, determine if this is a new, independent flaw or the result of an incomplete fix.
   - Research if the vulnerability is similar to known issues in other Git implementations, especially [Git itself](https://git-scm.com/) or [Git for Windows](https://gitforwindows.org/).
   - Consider if the vulnerability stems from a flaw in widely accepted semantics of Git repositories or common implementation patterns. If feasible, and on a best-effort basis, check experimentally if other Git implementations are affected.

4. **Coordinate with External Parties (if necessary)**:

   - **Other Git Projects**: If the vulnerability is confirmed or likely to affect other Git implementations, triage whether coordination is needed. For vulnerabilities in Git itself (including Git for Windows), [the git-security mailing list](https://git-scm.com/community#git-security) is the correct communication channel. We will investigate if this list is also appropriate for broader coordination and update this IRP accordingly.

   - **Consumers**: For severe vulnerabilities, consider if direct coordination with critical applications or libraries that use Gitoxide is warranted. This is expected to be rare.

   - **Downstream Packagers**: Coordination with downstream packagers is not currently a required step, as there are few. However, this may be a consideration in the future for high-risk issues.

5. **Update Advisory and Request CVE**:

   - Update the draft GitHub Security Advisory with all relevant findings, analysis, and references.
   - Once the nature of the vulnerability is understood, [request a CVE identifier](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/about-repository-security-advisories#cve-identification-numbers) through the [GitHub advisory interface](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/publishing-a-repository-security-advisory#requesting-a-cve-identification-number-optional).
   - *Note for "Issue Advisory Early": When following this strategy, we publish the initial public advisory once we fully understand the vulnerability, usually around the same time as we request the CVE. We may then update it with further findings to ensure it remains as useful as possible, and to maintain transparency.*

## Phase Three: Remediation and Disclosure

This phase covers developing and releasing a fix.

1. **Plan the Fix**: Design a code-level solution to address the vulnerability.

2. **Establish a Timeline**: If the fix is complex, break it down into manageable steps and establish a realistic timeline for implementation and release.

3. **Implement and Test**: Write the code for the fix. Ensure comprehensive tests are added to prevent regressions. Test any changes required in other Gitoxide crates to adapt to the fix.

4. **Finalize Advisory**: Perform a final review of the security advisory. Add the version numbers of all crates that will be released in the fix.

5. **Publish and Release**:

   - [Publish](https://docs.github.com/en/code-security/security-advisories/working-with-repository-security-advisories/publishing-a-repository-security-advisory#publishing-a-security-advisory) the GitHub Security Advisory (GHSA).
   - Simultaneously, release new versions of all affected crates and any other crates whose dependencies need to be bumped.
   - *Note for "Issue Advisory Early": This step becomes **updating the existing public advisory** with details about the fix, along with the release of the new crate versions.*

6. **Create RUSTSEC Advisory**: Author one or more advisories for the [`rustsec/advisory-db`](https://github.com/rustsec/advisory-db) repository. The content should be consistent with the GHSA. Open a [pull request](https://github.com/RustSec/advisory-db/blob/main/CONTRIBUTING.md) to submit them.

   - *Note for "Issue Advisory Early": A preliminary RUSTSEC advisory should be published along with the public GHSA or immediately thereafter, and likewise updated later with information about the fix.*

## Phase Four: Post-Disclosure Follow-up

After the fix is public, monitor its rollout and ensure information is accurate.

1. **Monitor for Breakages**: Keep an eye on user reports (e.g., GitHub issues) to see if the fix has introduced any breaking changes.

2. **Verify Public Advisories**: A few days after publication, check the global GHSA in the [GitHub Advisory Database](https://github.com/advisories) and the published [RUSTSEC](https://rustsec.org/advisories/) advisory. Ensure their content and formatting are correct and consistent with the original repository advisory.

3. **Update Advisories**: Update all advisories as needed with any new information, such as adding the CVE number if it was not available at the time of initial publication.

## Phase Five: Post-Incident Review

After the incident is fully resolved, it is crucial to learn from it.

1. **Process Retrospective**: Discuss the handling of the incident. Identify what went well and what could be improved in our response process. Such discussion may be brief or extensive, depending on the vulnerability and how involved it was to handle and remedy. Update this IRP with any lessons learned.

2. **Root Cause Analysis**:

   - Examine how the vulnerability was introduced and whether process or tooling changes could prevent similar issues in the future.
   - Assess if the vulnerability is a symptom of a broader architectural or design pattern in the software that needs to be reconsidered.
   - Consider if the vulnerability represents a condition previously thought to be benign, but whose security implications have grown due to evolving use cases and expectations. This may give insight into how to prioritize existing issues and requested features.
   - If the vulnerability arose from incorrect assumptions about portability, examine whether there are other areas of the code that embody the same or similar assumptions and can be improved.
