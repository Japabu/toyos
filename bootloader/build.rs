fn main() {
    println!("cargo::rerun-if-env-changed=INIT_PROGRAMS");
    if std::env::var("INIT_PROGRAMS").is_err() {
        println!("cargo::rustc-env=INIT_PROGRAMS=");
    }
}
