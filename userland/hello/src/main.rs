#![no_main]
#![feature(restricted_std)]

#[no_mangle]
pub extern "C" fn _start() -> ! {
    println!("Hello from ToyOS userland!");

    let v = vec![1, 2, 3, 4, 5];
    println!("Vec: {:?}", v);

    let s = String::from("ToyOS says hi!");
    println!("{}", s);

    toyos_api::toyos_exit(0);
}
