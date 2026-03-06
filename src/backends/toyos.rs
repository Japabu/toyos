#[inline(always)]
pub unsafe fn guess_os_stack_limit() -> Option<usize> {
    let (base, _size) = toyos_abi::syscall::stack_info()?;
    Some(base as usize)
}
