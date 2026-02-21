use std::io::{self, Write};

fn main() {
    println!("=== Input Test ===");
    println!("Type something and press Enter. Type 'quit' to exit.");
    println!();

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("> ");
        stdout.flush().unwrap();

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!("[EOF]");
                break;
            }
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed == "quit" {
                    println!("Bye!");
                    break;
                }
                println!("You typed: {:?} ({} bytes)", trimmed, trimmed.len());
            }
            Err(e) => {
                println!("Error: {}", e);
                break;
            }
        }
    }
}
