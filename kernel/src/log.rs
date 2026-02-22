#[cfg(not(test))]
pub fn println(s: &str) {
    crate::drivers::serial::println(s);
    crate::console::println(s);
}

#[cfg(test)]
pub fn println(s: &str) {
    std::println!("{}", s);
}
