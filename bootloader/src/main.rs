#![no_main]
#![no_std]

extern crate alloc;

use core::mem::size_of;

use alloc::vec;
use uefi::{
    prelude::*,
    proto::{
        loaded_image::LoadedImage,
        media::file::{File, FileAttribute, FileInfo, FileMode},
    },
    table::boot::MemoryType,
    CStr16,
};
use uefi_services::println;

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut system_table).unwrap();

    println!("Starting bootloader...");
    println!("Loading kernel...");
    // Load kernel
    let mut fs = system_table
        .boot_services()
        .get_image_file_system(handle)
        .expect("Failed to get file system");

    let mut kernel_file = fs
        .open_volume()
        .expect("Failed to open volume")
        .open(
            cstr16!("\\toyos\\kernel.elf"),
            FileMode::Read,
            Default::default(),
        )
        .expect("Failed to open kernel file")
        .into_regular_file()
        .expect("Failed to convert kernel file to regular file");

    let kernel_file_info_len = kernel_file
        .get_info::<FileInfo>(&mut [])
        .expect_err("Failed to get file info len")
        .data()
        .expect("File info len was None");

    let mut buffer = vec![0; kernel_file_info_len];
    let kernel_file_info = kernel_file
        .get_info::<FileInfo>(&mut buffer)
        .expect("Failed to get file info");

    println!("Kernel file size: {}", kernel_file_info.file_size());
    println!("Reading kernel...");

    let mut kernel_bytes = vec![0; kernel_file_info.file_size() as usize];
    kernel_file
        .read(&mut kernel_bytes)
        .expect("Failed to read kernel file");

    // let (_system_table, _memory_map) = system_table.exit_boot_services(MemoryType::CONVENTIONAL);

    loop {}
}
