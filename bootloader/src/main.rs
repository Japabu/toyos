#![no_main]
#![no_std]

use uefi::{prelude::*, table::boot::MemoryType};

#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut system_table).unwrap();

    uefi_services::println!("Hello, world!");

    // let (_system_table, _memory_map) = system_table.exit_boot_services(MemoryType::CONVENTIONAL);

    loop {}
}
