#[cfg(not(test))]
pub fn println(s: &str) {
    crate::serial::println(s);
}

#[cfg(test)]
pub fn println(s: &str) {
    std::println!("{}", s);
}
