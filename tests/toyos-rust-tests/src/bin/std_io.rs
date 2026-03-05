fn main() {
    // Basic print
    println!("hello from ToyOS");

    // Format strings
    let x = 42;
    let s = format!("value={x}");
    assert_eq!(s, "value=42");

    // String operations
    let greeting = String::from("hello ");
    let name = String::from("world");
    let combined = greeting + &name;
    assert_eq!(combined, "hello world");

    // Write to stderr
    eprintln!("stderr works too");
}
