pub fn main(_args: Vec<String>) {
    match std::env::current_dir() {
        Ok(path) => println!("{}", path.display()),
        Err(e) => eprintln!("pwd: {}", e),
    }
}
