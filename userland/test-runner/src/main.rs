use std::process::Command;

fn main() {
    let manifest = match std::fs::read_to_string("/initrd/test-manifest") {
        Ok(m) => m,
        Err(e) => {
            eprintln!("failed to read /initrd/test-manifest: {e}");
            std::process::exit(1);
        }
    };

    let mut passed = 0u32;
    let mut failed = 0u32;

    for line in manifest.lines() {
        let name = line.trim();
        if name.is_empty() || name.starts_with('#') {
            continue;
        }

        println!("===TEST_START {name}===");

        let path = format!("/initrd/{name}");
        match Command::new(&path).status() {
            Ok(status) => {
                let code = status.code().unwrap_or(-1);
                println!("===TEST_END {name} exit={code}===");
                if code == 0 {
                    passed += 1;
                } else {
                    failed += 1;
                }
            }
            Err(e) => {
                println!("===TEST_END {name} error={e}===");
                failed += 1;
            }
        }
    }

    println!("===ALL_TESTS_DONE passed={passed} failed={failed}===");
    std::process::exit(if failed > 0 { 1 } else { 0 });
}
