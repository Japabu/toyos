fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("args: {:?}", args);

    println!("--- alloc/free test ---");

    // Allocate a vec, print its address, drop it
    let v = vec![1u64, 2, 3, 4, 5];
    println!("v1 @ {:p}", v.as_ptr());
    drop(v);

    // Allocate same-sized vec — should reuse the freed address
    let v2 = vec![10u64, 20, 30, 40, 50];
    println!("v2 @ {:p} (should reuse v1)", v2.as_ptr());
    drop(v2);

    // Different sizes
    let a = Box::new([0u8; 128]);
    println!("a  @ {:p} (128 bytes)", a.as_ptr());
    drop(a);

    let b = Box::new([0u8; 128]);
    println!("b  @ {:p} (should reuse a)", b.as_ptr());
    drop(b);

    println!("--- done ---");
}
