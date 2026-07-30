fn main() {
    println!("hello from ToyOS");

    let x = 42;
    let s = format!("value={x}");
    assert_eq!(s, "value=42");

    let greeting = String::from("hello ");
    let name = String::from("world");
    let combined = greeting + &name;
    assert_eq!(combined, "hello world");

    eprintln!("stderr works too");
}
