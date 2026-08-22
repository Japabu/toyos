//! The instrument every guest in this project is measured with, declared once.
//!
//! The measurement this exists for: on one runner image, one commit and one
//! accelerator, `desktop_typing_damage` is red on
//! QEMU 8.2.2 and green on 11.0.3, and `usb_storage_shapes` with it. So the
//! QEMU version is not a detail of the environment — it decides verdicts, and
//! a job that does not say which one it ran produces a number nobody can
//! compare with another.
//!
//! `.github/qemu-version` is that declaration. CI reads it from
//! `.github/instrument.sh` and **reds** on a disagreement, because `debian:sid`
//! is a rolling release and the alternative is an instrument that moves out
//! from under every recorded measurement in silence. This host reads it and
//! **notes** a disagreement, because brew moves QEMU when it feels like it and
//! a build must not stop for that — but the dev host is where
//! `tests/audio-baseline.toml` was recorded, so it drifting is the same fact
//! about the same comparison and has to be visible.

use std::path::Path;
use std::process::Command;

/// The QEMU every guest in CI runs, and the one this project's recorded numbers
/// were taken on.
///
/// Comment lines and blanks are stripped, so the file can explain itself to the
/// next reader; `.github/instrument.sh` strips the same two things with `grep`
/// and `tr`.
pub fn declared_qemu_version(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(".github/qemu-version")).ok()?;
    let version: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("")
        .split_whitespace()
        .collect();
    (!version.is_empty()).then_some(version)
}

/// What `qemu-system-x86_64 --version` says, or `None` where it did not answer
/// in the shape this reads.
///
/// One `--version` run and not a package query: the binary on `PATH` is the one
/// a boot will use, and no packaging system on any of the three hosts this runs
/// on answers for that.
pub fn host_qemu_version() -> Option<String> {
    let out = Command::new("qemu-system-x86_64").arg("--version").output().ok()?;
    parse_qemu_version(&String::from_utf8_lossy(&out.stdout))
}

/// `QEMU emulator version 11.0.3 (Debian 1:11.0.3+ds-1)` → `11.0.3`.
fn parse_qemu_version(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let rest = first.strip_prefix("QEMU emulator version ")?;
    let version = rest.split_whitespace().next()?;
    (!version.is_empty()).then(|| version.to_string())
}

/// The line `cargo run` prints when this host is not the instrument the
/// project's numbers were taken on, and nothing at all when it is.
pub fn qemu_version_note(root: &Path) -> Option<String> {
    let want = declared_qemu_version(root)?;
    let have = host_qemu_version()?;
    (have != want).then(|| {
        format!(
            "Note: this host runs QEMU {have} and .github/qemu-version declares {want} — \
             CI's guests and tests/audio-baseline.toml are on {want}, and the QEMU version \
             has been measured to decide test outcomes (`desktop_typing_damage` and \
             `usb_storage_shapes` are red on 8.2.2 and green on 11.0.3, same image, same \
             commit, same accelerator). Nothing here is broken; a comparison across the two is."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The workflows whose verdict somebody acts on. `probe-*.yml` are
    /// throwaway measurement branches and are not
    /// on this list; `toolchain.yml` installs QEMU for `check_prerequisites`
    /// and boots nothing.
    const GATES: &[&str] = &["ci.yml", "gate-a.yml"];

    /// Every `<job>:` block of a workflow, crudely and on purpose.
    ///
    /// Deliberately not a YAML parser: the shape is fixed — two spaces, a
    /// name, a colon, end of line — and anything else is a file to name
    /// rather than a shape to accommodate.
    fn jobs(text: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut in_jobs = false;
        for line in text.lines() {
            if line == "jobs:" {
                in_jobs = true;
                continue;
            }
            if !in_jobs {
                continue;
            }
            let is_header = line.starts_with("  ")
                && !line.starts_with("   ")
                && line.ends_with(':')
                && line[2..line.len() - 1].chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
            if is_header {
                out.push((line[2..line.len() - 1].to_string(), String::new()));
            } else if let Some(last) = out.last_mut() {
                last.1.push_str(line);
                last.1.push('\n');
            }
        }
        out
    }

    /// A job that installs QEMU is a job that boots a guest, and one that boots
    /// a guest without naming its instrument produces a verdict nobody can
    /// compare with another.
    ///
    /// The rule is here rather than in one workflow's review because the way
    /// this hides is that a workflow reads perfectly well and never says what
    /// it is comparing against — gate A ran QEMU 8.2.2 against every other
    /// guest in CI on 11.0.3 for as long as that file existed.
    fn nameless(text: &str) -> Vec<String> {
        jobs(text)
            .into_iter()
            .filter(|(_, body)| body.contains("qemu-system-x86"))
            .filter(|(_, body)| !body.contains("instrument.sh"))
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn every_gate_that_boots_a_guest_names_its_instrument() {
        let root = repo_root();
        let mut bad = Vec::new();
        for file in GATES {
            let path = root.join(".github/workflows").join(file);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} is a gate and is not readable: {e}", path.display()));
            // A scan that found no job at all would report every gate clean,
            // which is the shape this rule exists to refuse.
            let booting = jobs(&text).into_iter().filter(|(_, b)| b.contains("qemu-system-x86"));
            assert!(
                booting.count() > 0,
                "{file} is on the list because it boots guests, and the job scan found none — \
                 the scan is wrong, or the file no longer belongs on it"
            );
            for job in nameless(&text) {
                bad.push(format!("{file}: `{job}` installs QEMU and never runs instrument.sh"));
            }
        }
        assert!(
            bad.is_empty(),
            "a job that boots a guest without declaring its QEMU is a third instrument, \
             and that is invisible in a diff:\n  {}",
            bad.join("\n  ")
        );
    }

    /// Teeth, run rather than argued: the tree cannot contain the workflow this
    /// rule is written against, so the rule is shown to refuse one.
    #[test]
    fn the_job_scan_refuses_a_job_that_boots_without_saying_what_with() {
        let good = concat!(
            "jobs:\n",
            "  a:\n    steps:\n",
            "      - run: apt-get install qemu-system-x86\n",
            "      - run: .github/instrument.sh\n",
        );
        assert!(nameless(good).is_empty());

        let bad = concat!(
            "jobs:\n",
            "  a:\n    steps:\n      - run: .github/instrument.sh\n",
            "  b:\n    steps:\n      - run: apt-get install qemu-system-x86\n",
        );
        assert_eq!(nameless(bad), ["b"]);

        assert_eq!(jobs(bad).iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    /// The other declaration a workflow matches on by hand.
    ///
    /// `ci.yml`'s `durations` job renders the price verdict only where the
    /// profile was measured: on a T14 lane it matches
    /// [`crate::durations::TIER_DISAGREEMENT`] in the merge output, prints it
    /// as a warning and exits 0, and every other way `--merge-durations` can
    /// refuse still reds. That telling-apart is a string in a shell script
    /// against a string in Rust, and the failure mode is silent in exactly one
    /// direction — reword the panic and the workflow stops recognising the one
    /// verdict it is allowed to soften, while both files still read perfectly.
    /// So the two are held together here.
    ///
    /// The job's own guard is the same shape: `route.yml`'s `trusted` output is
    /// what says where the lane ran, and `runner.name` would answer about the
    /// hosted machine this job itself runs on.
    #[test]
    fn the_softened_duration_verdict_is_the_one_the_merge_actually_raises() {
        let path = repo_root().join(".github/workflows/ci.yml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is a gate and is not readable: {e}", path.display()));
        assert!(
            text.contains(crate::durations::TIER_DISAGREEMENT),
            "ci.yml no longer matches {:?}, so its `durations` job either fails a T14 lane on \
             a verdict that instrument cannot render, or softens a refusal that is not the \
             price verdict",
            crate::durations::TIER_DISAGREEMENT
        );
        let (_, durations) = jobs(&text)
            .into_iter()
            .find(|(name, _)| name == "durations")
            .expect("ci.yml renders the duration verdict in a job called `durations`");
        assert!(
            durations.contains("needs.route.outputs.trusted"),
            "the `durations` job stopped reading where the guest lane ran, so it renders the \
             price verdict against a profile the measuring machine may never have taken"
        );
        assert!(
            !text.contains("${{ runner.name"),
            "where a lane ran is `route.yml`'s answer, not the name of the runner reading it"
        );
    }

    /// `route.yml`'s `HOSTED` expression, as one whitespace-normalised line.
    ///
    /// Not a YAML parser, for the reason [`jobs`] is not one: the shape is
    /// fixed — a `HOSTED: >-` key and a folded block indented under it — and
    /// anything else is a file to name rather than a shape to accommodate.
    fn hosted_expression(text: &str) -> String {
        let mut lines = text.lines().skip_while(|l| !l.trim_start().starts_with("HOSTED:"));
        let head = lines.next().expect("route.yml declares HOSTED");
        let indent = head.len() - head.trim_start().len();
        let mut expr = String::new();
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            if line.len() - line.trim_start().len() <= indent {
                break;
            }
            expr.push(' ');
            expr.push_str(line.trim());
        }
        expr.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Every event name `route.yml`'s `HOSTED` expression tests for.
    fn hosted_events(expr: &str) -> BTreeSet<String> {
        expr.split("github.event_name == '")
            .skip(1)
            .filter_map(|rest| rest.split('\'').next())
            .map(str::to_string)
            .collect()
    }

    /// Where a Linux job runs is one expression in one file, and this is what
    /// it has to say.
    ///
    /// `route.yml`'s `HOSTED` decides the whole repository's routing and every
    /// consumer reads the answer back through `needs.route.outputs.trusted`, so
    /// a clause dropped from it is invisible in every other file. The direction
    /// that hurts is the one that puts branch traffic back on the T14: that
    /// machine has one runner with one worker, and on 2026-08-22T05:03Z
    /// thirteen runs — seven `toolchain`, five `ci`, one `landing`, all of them
    /// `pull_request` or `push` from this repository's own branches — were
    /// queued behind one scheduled gate A, while `toolchain.yml`'s `build`, a
    /// required check, spent 57 minutes in that queue and then failed in nine
    /// seconds (run 32549542807).
    ///
    /// So the events are pinned as a set rather than as four `contains` calls:
    /// a clause added here is as much a routing change as one removed, and the
    /// set is what says which.
    #[test]
    fn every_pull_request_and_push_routes_to_the_hosted_lane() {
        let path = repo_root().join(".github/workflows/route.yml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} decides where CI runs: {e}", path.display()));
        let expr = hosted_expression(&text);

        assert_eq!(
            hosted_events(&expr),
            ["merge_group", "pull_request", "push", "schedule"]
                .into_iter()
                .map(str::to_string)
                .collect::<BTreeSet<_>>(),
            "route.yml's HOSTED names a different set of events than the routing rule: \
             {expr:?}"
        );
        assert!(
            expr.contains("github.event_name == 'schedule' && github.workflow == 'ci'"),
            "the `schedule` clause has to stay `ci.yml`'s alone — gate A's and \
             portability's schedules are the T14's: {expr:?}"
        );
        assert!(
            !expr.contains("head.repo"),
            "HOSTED tells a pull request apart by where its head is, so a same-repo pull \
             request is on the T14 again — which is the queue this rule was rewritten to \
             empty: {expr:?}"
        );
        assert!(
            !expr.contains("workflow_dispatch"),
            "a dispatch is the T14's manual lane, and this expression naming it means \
             nothing routes to the machine but a schedule: {expr:?}"
        );
    }

    /// The scan, shown refusing the shape it is written against.
    #[test]
    fn the_hosted_scan_reads_the_clauses_and_not_the_prose_around_them() {
        let file = concat!(
            "        env:\n",
            "          # push is a comment here and not a clause\n",
            "          HOSTED: >-\n",
            "            ${{ github.event_name == 'merge_group'\n",
            "                || github.event_name == 'pull_request' }}\n",
            "        run: |\n",
            "          echo github.event_name == 'schedule'\n",
        );
        let expr = hosted_expression(file);
        assert_eq!(
            expr,
            "${{ github.event_name == 'merge_group' || github.event_name == 'pull_request' }}"
        );
        assert_eq!(
            hosted_events(&expr),
            ["merge_group", "pull_request"].into_iter().map(str::to_string).collect()
        );
    }

    /// The declaration is read by a shell and by this crate, so both have to
    /// agree that it holds one version and nothing else.
    #[test]
    fn the_declared_version_is_a_version() {
        let declared =
            declared_qemu_version(&repo_root()).expect(".github/qemu-version declares a version");
        assert!(
            declared.split('.').count() >= 2
                && declared.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "{declared:?} is not a QEMU version"
        );
    }

    #[test]
    fn the_script_that_reads_it_is_runnable() {
        let path = repo_root().join(".github/instrument.sh");
        let meta = std::fs::metadata(&path).expect(".github/instrument.sh is there");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(
                meta.permissions().mode() & 0o111 != 0,
                "every guest job invokes it by path, so a lost exec bit reds all thirteen"
            );
        }
        let _ = meta;
    }

    #[test]
    fn the_version_parser_takes_what_qemu_prints_and_refuses_the_rest() {
        assert_eq!(
            parse_qemu_version("QEMU emulator version 11.0.3 (Debian 1:11.0.3+ds-1)\n").as_deref(),
            Some("11.0.3")
        );
        assert_eq!(
            parse_qemu_version("QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.11)\n")
                .as_deref(),
            Some("8.2.2")
        );
        assert_eq!(parse_qemu_version("qemu-system-x86_64: no such option\n"), None);
        assert_eq!(parse_qemu_version(""), None);
    }
}
