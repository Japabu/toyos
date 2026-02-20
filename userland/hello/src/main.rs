fn main() {
    println!("Hello from ToyOS userland!");

    let v = vec![1, 2, 3, 4, 5, 7, 8];
    println!("Vec: {:?}", v);

    let s = String::from("ToyOS says hi!");
    println!("{}", s);
}
