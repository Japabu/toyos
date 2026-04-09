use std::process::Command;

fn main() {
    let version = Command::new(if cfg!(windows) { "git.exe" } else { "git" })
        .args(["describe", r"--match=v*\.*\.*"])
        .output()
        .ok()
        .and_then(|out| {
            if !out.status.success() {
                return None;
            }
            try_parse_describe(&out.stdout)
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
    println!("cargo:rustc-env=GIX_VERSION={version}");
}

fn try_parse_describe(input: &[u8]) -> Option<String> {
    let input = std::str::from_utf8(input).ok()?;
    let trimmed = input.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}
