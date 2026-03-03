#![feature(c_variadic)]
#![allow(non_camel_case_types, dead_code, unused_variables, unused_assignments, unused_unsafe)]

mod ctype;
mod math;
mod memory;
mod printf;
mod stdio;
mod string;

// Force all modules to be linked in even if the Rust side doesn't reference them.
pub use ctype::_libc_ctype_init;
pub use math::_libc_math_init;
pub use memory::_libc_memory_init;
pub use printf::_libc_printf_init;
pub use stdio::_libc_stdio_init;
pub use string::_libc_string_init;
