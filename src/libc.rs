use std::fs;
use std::path::Path;
use std::process::Command;

use crate::stamps;
use crate::toolchain::host_triple;

/// Ensure toyos-libc is built and installed as libtoyos_c.a in the sysroot.
pub fn ensure(root: &Path, rust_dir: &Path) {
    let libc_src = root.join("userland/libc/src");
    let stamp = root.join("target/stamps/toyos-libc.stamp");
    let host = host_triple();
    let sysroot_lib = rust_dir.join(format!(
        "build/{host}/stage2/lib/rustlib/x86_64-unknown-toyos/lib"
    ));
    let dest = sysroot_lib.join("libtoyos_c.a");

    let source_changed = stamps::dir_changed(&libc_src, &stamp);

    if !source_changed && dest.exists() {
        return;
    }

    eprintln!("Building toyos-libc for sysroot...");

    // Build with --message-format=json to discover exact rlib artifacts
    let output = Command::new("cargo")
        .args([
            "+toyos",
            "build",
            "--release",
            "--target",
            "x86_64-unknown-toyos",
            "--features",
            "std-runtime",
            "--message-format=json",
            "--manifest-path",
        ])
        .arg(root.join("userland/libc/Cargo.toml").to_str().unwrap())
        .current_dir(root.join("userland"))
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("Failed to build toyos-libc");
    if !output.status.success() {
        // --message-format=json suppresses rendered errors on stderr.
        // Print them from the JSON diagnostic messages on stdout.
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if msg.get("reason").and_then(|r| r.as_str()) == Some("compiler-message") {
                if let Some(rendered) = msg.pointer("/message/rendered").and_then(|r| r.as_str()) {
                    eprint!("{rendered}");
                }
            }
        }
        panic!("toyos-libc build failed");
    }

    // Collect rlib paths from cargo's JSON output, then merge into a single archive.
    let mut rlib_paths: Vec<std::path::PathBuf> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let Some(filenames) = msg.get("filenames").and_then(|f| f.as_array()) else { continue };
        for f in filenames {
            let Some(path) = f.as_str() else { continue };
            if path.ends_with(".rlib") {
                rlib_paths.push(std::path::PathBuf::from(path));
            }
        }
    }
    assert!(!rlib_paths.is_empty(), "No rlib artifacts found for toyos-libc");

    let archive = merge_rlibs(&rlib_paths);
    fs::write(&dest, archive)
        .unwrap_or_else(|e| panic!("Failed to write {}: {e}", dest.display()));
    stamps::write_dir_stamp(&libc_src, &stamp);
}

/// Extract .o files from rlibs and merge them into a single GNU-format ar archive.
fn merge_rlibs(rlib_paths: &[std::path::PathBuf]) -> Vec<u8> {
    let mut members: Vec<(String, Vec<u8>)> = Vec::new();
    for path in rlib_paths {
        let data = fs::read(path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
        extract_rlib_objects(&data, &mut members);
    }

    // Build GNU string table for names > 15 chars
    let mut string_table = Vec::new();
    let mut name_refs: Vec<String> = Vec::new();
    for (name, _) in &members {
        let ar_name = format!("{name}/");
        if ar_name.len() <= 16 {
            name_refs.push(ar_name);
        } else {
            let offset = string_table.len();
            string_table.extend_from_slice(ar_name.as_bytes());
            string_table.push(b'\n');
            name_refs.push(format!("/{offset}"));
        }
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(b"!<arch>\n");

    // Write string table if needed
    if !string_table.is_empty() {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            "//", "0", "0", "0", "100644", string_table.len()
        );
        buf.extend_from_slice(header.as_bytes());
        buf.extend_from_slice(&string_table);
        if string_table.len() % 2 == 1 {
            buf.push(b'\n');
        }
    }

    for (i, (_name, data)) in members.iter().enumerate() {
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            &name_refs[i], "0", "0", "0", "100644", data.len()
        );
        buf.extend_from_slice(header.as_bytes());
        buf.extend_from_slice(data);
        if data.len() % 2 == 1 {
            buf.push(b'\n');
        }
    }
    buf
}

/// Extract .o members from an rlib (ar archive), skipping .rmeta and special members.
fn extract_rlib_objects(data: &[u8], out: &mut Vec<(String, Vec<u8>)>) {
    assert!(
        data.starts_with(b"!<arch>\n"),
        "not an ar archive"
    );
    let mut pos = 8;

    // First pass: find GNU string table (member named "//")
    let mut string_table: &[u8] = &[];
    {
        let mut scan = 8;
        while scan + 60 <= data.len() {
            let header = &data[scan..scan + 60];
            let size: usize = std::str::from_utf8(&header[48..58])
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if header[..16].starts_with(b"//") {
                string_table = &data[scan + 60..scan + 60 + size];
                break;
            }
            scan += 60 + size;
            if scan % 2 == 1 {
                scan += 1;
            }
        }
    }

    // Second pass: extract .o members
    while pos + 60 <= data.len() {
        let header = &data[pos..pos + 60];
        let name_field = &header[0..16];
        let size: usize = std::str::from_utf8(&header[48..58])
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let member_data = &data[pos + 60..pos + 60 + size];

        let name = if name_field.starts_with(b"/") && name_field[1].is_ascii_digit() {
            // GNU long name: /offset into string table
            let offset: usize = std::str::from_utf8(&name_field[1..])
                .unwrap()
                .trim_end_matches('/')
                .trim()
                .parse()
                .unwrap();
            let end = string_table[offset..]
                .iter()
                .position(|&b| b == b'/' || b == b'\n')
                .map(|i| offset + i)
                .unwrap_or(string_table.len());
            String::from_utf8_lossy(&string_table[offset..end]).to_string()
        } else if name_field.starts_with(b"#1/") {
            // BSD long name: #1/<length> with name prepended to data
            let name_len: usize = std::str::from_utf8(&name_field[3..])
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            String::from_utf8_lossy(&member_data[..name_len])
                .trim_end_matches('\0')
                .to_string()
        } else {
            std::str::from_utf8(name_field)
                .unwrap()
                .trim_end_matches('/')
                .trim()
                .to_string()
        };

        pos += 60 + size;
        if pos % 2 == 1 {
            pos += 1;
        }

        if name == "/"
            || name == "//"
            || name == "__.SYMDEF"
            || name == "__.SYMDEF SORTED"
            || name.ends_with(".rmeta")
            || name.is_empty()
        {
            continue;
        }
        if name.ends_with(".o") {
            out.push((name, member_data.to_vec()));
        }
    }
}
