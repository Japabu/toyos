//! Applying relocations to a loaded module.
//!
//! Every write here goes through [`LoadedLib::write_at`], and every offset it
//! is given was validated against the module's writable window by
//! `load_shared_lib` before the module existed. That is why the asserts in
//! `write_at` are kernel-bug asserts and not refusals: reaching one means the
//! validation let something through.
//!
//! Unresolved symbols are logged and left alone, never fatal. A `.so` naming a
//! symbol nothing defines is a malformed file, and the process faults on the
//! slot if it ever uses it — which is userland's problem, in userland.

use super::{CachedRelocs, LibMemory, LoadedLib, TlsModule, TlsModuleInfo};
use crate::UserAddr;
use toyos_elf::RelocKind;

impl LoadedLib {
    /// Write a value at a byte offset within this module's kernel mapping.
    ///
    /// `offset` is a relocation's `r_offset`, i.e. it came out of the file.
    /// Each arm asserts the bound protecting *its own* destination, which for
    /// the `Shared` arm is `rw_alloc` — a separate, smaller allocation, so the
    /// image's bounds do not cover it.
    ///
    /// # Safety
    /// The bound each arm needs is checked here, at runtime, on every call —
    /// that is this module's whole design (its own header: the asserts are
    /// "kernel-bug asserts, not refusals", because `load_shared_lib`/
    /// `rela::validate` already refused a malformed `r_offset` before any
    /// `LoadedLib` existed). What is *not* checked is exclusivity: the
    /// caller must be the only writer of this module's image (or its
    /// `rw_alloc`) for the duration of the call — true for every current
    /// caller, each of which runs during a module's single-threaded loading/
    /// binding phase (spawn, `dlopen`), never concurrently with another
    /// relocation pass over the same `LoadedLib`.
    pub(super) unsafe fn write_at<T: Copy>(&self, offset: u64, value: T) {
        let end = (offset as usize)
            .checked_add(core::mem::size_of::<T>())
            .expect("LoadedLib::write_at: r_offset + width overflows");
        match &self.memory {
            LibMemory::Owned(_) => {
                assert!(
                    end <= self.image.size(),
                    "LoadedLib::write_at: r_offset {:#x} outside image of {:#x}",
                    offset,
                    self.image.size()
                );
                self.image.write(offset as usize, value)
            }
            LibMemory::Shared { rw_alloc, rw_offset, rw_delta, .. } => {
                assert!(
                    offset as usize >= *rw_offset && end <= *rw_offset + rw_alloc.size(),
                    "LoadedLib::write_at: r_offset {:#x} outside the writable window [{:#x}, {:#x})",
                    offset,
                    rw_offset,
                    rw_offset + rw_alloc.size()
                );
                let ptr = (self.image.base().add(offset as usize) as i64 + rw_delta) as *mut T;
                ptr.write_unaligned(value);
            }
        }
    }

    /// The pre-scanned entries of one kind, or the module's own tables when it
    /// was never cached.
    ///
    /// The two paths differ only in cost. A cached module's tables are scanned
    /// once at cache time; an uncached one is scanned per use.
    fn bind_entries(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        let cached = self.cached_relocs.as_ref().map(|r| r.bind.iter().copied());
        let scanned = self.cached_relocs.is_none().then(|| {
            self.relocations()
                .filter(|r| r.kind.is_bind())
                .map(|r| (r.offset, r.sym))
        });
        cached.into_iter().flatten().chain(scanned.into_iter().flatten())
    }

    fn typed_entries(
        &self,
        kind: RelocKind,
        pick: fn(&CachedRelocs) -> &alloc::vec::Vec<(u64, u32, i64)>,
    ) -> impl Iterator<Item = (u64, u32, i64)> + '_ {
        let cached = self.cached_relocs.as_ref().map(|r| pick(r).iter().copied());
        let scanned = self.cached_relocs.is_none().then(move || {
            self.relocations()
                .filter(move |r| r.kind == kind)
                .map(|r| (r.offset, r.sym, r.addend))
        });
        cached.into_iter().flatten().chain(scanned.into_iter().flatten())
    }
}

/// Add `delta` to every `R_X86_64_RELATIVE` slot.
///
/// Called once the module has been given a user address, which differs from
/// the physical base `load_shared_lib` applied them with.
///
/// Reads the old value out of the shared image rather than the private
/// writable window: the only state this is ever called in is a freshly cloned
/// window, which is a byte-for-byte copy of the image's.
pub fn rebase_relative_relocs(lib: &LoadedLib, delta: i64) {
    for r in lib.relocations() {
        if r.kind == RelocKind::Relative {
            // SAFETY: `r.offset` is a `RELATIVE` entry's offset, already
            // checked by `rela::validate` against the writable window at
            // `load_shared_lib` time (module header) — a range inside
            // `image`'s own bounds, per `write_at`'s `# Safety`. No
            // concurrent writer: this runs only on a freshly cloned window
            // nothing else has touched yet (this function's own doc).
            let old = unsafe { lib.image.read::<u64>(r.offset as usize) };
            // SAFETY: see `write_at`'s `# Safety` — `r.offset` was validated
            // the same way as the read just above.
            unsafe { lib.write_at::<u64>(r.offset, (old as i64 + delta) as u64) };
        }
    }
}

/// Bind a `dlopen`ed module's `GLOB_DAT`/`JUMP_SLOT` slots to symbols the
/// process already has.
pub fn resolve_dlopen_relocs(lib: &LoadedLib, other_libs: &[LoadedLib]) {
    let symbols = lib.symbols();
    let mut resolved = 0u64;
    let mut unresolved = 0u64;
    for (offset, sym) in lib.bind_entries() {
        let name = symbols.name(sym as usize);
        match other_libs.iter().find_map(|other| other.resolve(name)) {
            Some(addr) => {
                // SAFETY: `offset` came from `lib.bind_entries()`, one of
                // `load_shared_lib`'s `rela::validate`d tables — see
                // `write_at`'s `# Safety`.
                unsafe { lib.write_at::<u64>(offset, addr.raw()) };
                resolved += 1;
            }
            None => {
                if unresolved < 5 {
                    log!("dlopen: unresolved: {}", name);
                }
                unresolved += 1;
            }
        }
    }
    log!("dlopen: resolved {} relocs, {} unresolved", resolved, unresolved);
}

/// Bind a startup library's `GLOB_DAT`/`JUMP_SLOT` slots, preferring the
/// executable's own exports.
pub fn resolve_lib_bind_relocs(
    lib: &LoadedLib,
    exe_sym_map: &hashbrown::HashMap<&str, UserAddr>,
    libs: &[LoadedLib],
) {
    let symbols = lib.symbols();
    for (offset, sym) in lib.bind_entries() {
        let name = symbols.name(sym as usize);
        let resolved = exe_sym_map
            .get(name)
            .copied()
            .or_else(|| libs.iter().find_map(|other| other.resolve(name)));
        match resolved {
            // SAFETY: same as `resolve_dlopen_relocs` above — `offset` came
            // from `lib.bind_entries()`, validated the same way.
            Some(addr) => unsafe { lib.write_at::<u64>(offset, addr.raw()) },
            None => log!("dynamic: lib unresolved symbol: {}", name),
        }
    }
}

/// Apply `R_X86_64_TPOFF64` and `R_X86_64_TPOFF32`: the initial-exec model,
/// where a TLS reference is a fixed offset from the thread pointer.
///
/// `lib_base_offset` is this module's placement within the combined block and
/// `total_memsz` the whole block's size; the linker computes
/// `TPOFF = offset - memsz`, so both are needed.
pub fn apply_tpoff_relocs(
    lib: &LoadedLib,
    lib_base_offset: usize,
    total_memsz: usize,
    tls_info: &TlsModuleInfo,
) {
    let mut count64 = 0u64;
    for (offset, sym, addend) in lib.typed_entries(RelocKind::Tpoff64, |r| &r.tpoff64) {
        let tpoff = compute_tpoff(lib, sym, addend, lib_base_offset, total_memsz, tls_info);
        // SAFETY: `offset` came from `lib.typed_entries(RelocKind::Tpoff64,
        // ...)`, one of `load_shared_lib`'s `rela::validate`d tables — see
        // `write_at`'s `# Safety`.
        unsafe { lib.write_at::<u64>(offset, tpoff as u64) };
        count64 += 1;
    }
    let mut count32 = 0u64;
    for (offset, sym, addend) in lib.typed_entries(RelocKind::Tpoff32, |r| &r.tpoff32) {
        let tpoff = compute_tpoff(lib, sym, addend, lib_base_offset, total_memsz, tls_info);
        // SAFETY: same as the TPOFF64 loop above, for `RelocKind::Tpoff32`.
        unsafe { lib.write_at::<i32>(offset, tpoff as i32) };
        count32 += 1;
    }
    if count64 > 0 || count32 > 0 {
        log!(
            "dlopen: applied {} TPOFF64 + {} TPOFF32 relocs (base_offset={}, total_memsz={})",
            count64, count32, lib_base_offset, total_memsz
        );
    }
}

/// Apply `R_X86_64_DTPMOD64` and `R_X86_64_DTPOFF64`: the general-dynamic
/// model, where a TLS reference is a (module id, offset) pair
/// `__tls_get_addr` turns into an address through the DTV.
pub fn apply_dtpmod_relocs(lib: &LoadedLib, module_id: u64, tls_info: &TlsModuleInfo) {
    let mut count_mod = 0u64;
    for (offset, sym, _) in lib.typed_entries(RelocKind::DtpMod64, |r| &r.dtpmod64) {
        let mid = resolve_dtpmod(lib, sym, module_id, tls_info);
        // SAFETY: `offset` came from `lib.typed_entries(RelocKind::DtpMod64,
        // ...)`, one of `load_shared_lib`'s `rela::validate`d tables — see
        // `write_at`'s `# Safety`.
        unsafe { lib.write_at::<u64>(offset, mid) };
        count_mod += 1;
    }
    let mut count_off = 0u64;
    for (offset, sym, addend) in lib.typed_entries(RelocKind::DtpOff64, |r| &r.dtpoff64) {
        let value = resolve_dtpoff(lib, sym, addend, tls_info);
        // SAFETY: same as the DTPMOD64 loop above, for `RelocKind::DtpOff64`.
        unsafe { lib.write_at::<u64>(offset, value as u64) };
        count_off += 1;
    }
    if count_mod > 0 || count_off > 0 {
        log!(
            "dlopen: applied {} DTPMOD64 + {} DTPOFF64 relocs (module_id={})",
            count_mod, count_off, module_id
        );
    }
}

/// The module in `tls_info` that defines a TLS symbol, matched by template
/// pointer — unique per module, since each points into a distinct image.
fn defining_module<'a>(name: &str, tls_info: &'a TlsModuleInfo) -> Option<(&'a TlsModule, u64)> {
    for lib in tls_info.libs {
        if lib.tls_memsz == 0 {
            continue;
        }
        if let Some(sym_offset) = lib.resolve_tls(name) {
            let module = tls_info
                .modules
                .iter()
                .find(|m| m.template == lib.tls_template)?;
            return Some((module, sym_offset));
        }
    }
    None
}

/// Which module id a `DTPMOD64` slot gets.
///
/// An undefined TLS symbol that no module defines is a `.so` naming something
/// that is not there, which `dlopen` puts squarely on the untrusted side. Every
/// other unresolved-symbol path in this module logs and leaves the slot for
/// userland to fault on; these two used to panic the kernel instead.
fn resolve_dtpmod(lib: &LoadedLib, r_sym: u32, self_module_id: u64, tls_info: &TlsModuleInfo) -> u64 {
    if r_sym == 0 {
        return self_module_id;
    }
    let symbols = lib.symbols();
    if symbols.get(r_sym as usize).is_some_and(|s| s.is_defined()) {
        return self_module_id;
    }
    let name = symbols.name(r_sym as usize);
    match defining_module(name, tls_info) {
        Some((module, _)) => module.module_id,
        None => {
            log!("dtpmod: unresolved TLS symbol: {}", name);
            self_module_id
        }
    }
}

/// The offset a `DTPOFF64` slot gets: within the *defining* module's TLS
/// segment, since `__tls_get_addr` adds it to that module's block.
fn resolve_dtpoff(lib: &LoadedLib, r_sym: u32, r_addend: i64, tls_info: &TlsModuleInfo) -> i64 {
    if r_sym == 0 {
        return r_addend;
    }
    let symbols = lib.symbols();
    if let Some(sym) = symbols.get(r_sym as usize).filter(|s| s.is_defined()) {
        return sym.value as i64 + r_addend;
    }
    let name = symbols.name(r_sym as usize);
    match defining_module(name, tls_info) {
        Some((_, sym_offset)) => sym_offset as i64 + r_addend,
        None => {
            log!("dtpoff: unresolved TLS symbol: {}", name);
            r_addend
        }
    }
}

/// The thread-pointer-relative offset a `TPOFF` slot gets.
fn compute_tpoff(
    lib: &LoadedLib,
    r_sym: u32,
    r_addend: i64,
    lib_base_offset: usize,
    total_memsz: usize,
    tls_info: &TlsModuleInfo,
) -> i64 {
    if r_sym == 0 {
        return lib_base_offset as i64 + r_addend - total_memsz as i64;
    }
    let symbols = lib.symbols();
    if let Some(sym) = symbols.get(r_sym as usize).filter(|s| s.is_defined()) {
        return lib_base_offset as i64 + sym.value as i64 + r_addend - total_memsz as i64;
    }
    let name = symbols.name(r_sym as usize);
    match defining_module(name, tls_info) {
        Some((module, sym_offset)) => {
            module.base_offset as i64 + sym_offset as i64 - total_memsz as i64
        }
        None => {
            log!("tpoff: unresolved TLS symbol: {}", name);
            0
        }
    }
}
