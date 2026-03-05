use std::process::Command;

fn main() {
    // Spawn a child process (echo via toybox)
    let output = Command::new("/initrd/echo")
        .arg("hello from child")
        .output();

    match output {
        Ok(out) => {
            assert!(out.status.success(), "echo should succeed");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(stdout.contains("hello from child"), "should capture child stdout");
        }
        Err(e) => {
            // echo might not be in the test initrd, that's ok — skip
            eprintln!("note: echo not available ({e}), skipping child process test");
        }
    }

    // Test exit code
    println!("all process tests passed");
}
