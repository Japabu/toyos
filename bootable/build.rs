fn main() {
    println!("cargo::rustc-check-cfg=cfg(release)");
    if std::env::var("PROFILE").unwrap() == "release" {
        println!("cargo::rustc-cfg=release");
    }
}
