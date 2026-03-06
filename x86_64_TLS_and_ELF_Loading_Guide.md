# x86_64 Thread-Local Storage (TLS) Linking and ELF Loading: A Technical Deep Dive

## Table of Contents

1. [Introduction](#1-introduction)
2. [ELF Loading on x86_64](#2-elf-loading-on-x86_64)
3. [TLS Fundamentals](#3-tls-fundamentals)
4. [TLS Data Structure Layout (Variant II)](#4-tls-data-structure-layout-variant-ii)
5. [The Four TLS Access Models](#5-the-four-tls-access-models)
6. [TLS Relocation Types on x86_64](#6-tls-relocation-types-on-x86_64)
7. [Linker TLS Optimization (Relaxation)](#7-linker-tls-optimization-relaxation)
8. [Runtime TLS Resolution: `__tls_get_addr`](#8-runtime-tls-resolution-__tls_get_addr)
9. [TLSDESC: An Alternative Calling Convention](#9-tlsdesc-an-alternative-calling-convention)
10. [Kernel and libc Bootstrap](#10-kernel-and-libc-bootstrap)
11. [Practical Examples](#11-practical-examples)
12. [References](#12-references)

---

## 1. Introduction

Thread-Local Storage (TLS) allows programs to declare variables that have a unique instance per thread, using the `__thread` keyword in C/C++ (or `thread_local` in C11/C++11). Implementing TLS efficiently requires coordinated effort across the entire toolchain: the compiler, the static linker, the dynamic linker (runtime linker), the C library, and the kernel.

The canonical reference for ELF TLS is Ulrich Drepper's *ELF Handling For Thread-Local Storage* (2003), which defines the data structures, access models, and relocation types used across architectures. This guide focuses exclusively on the **x86_64 (AMD64)** architecture running **Linux** with **glibc**.

TLS variables are declared in C like:

```c
__thread int my_counter;
__thread char my_buffer[256];
static __thread int file_local;
```

At the ELF level, these variables reside in special sections (`.tdata` for initialized data, `.tbss` for zero-initialized data) and are described by a `PT_TLS` program header. The runtime system ensures each thread gets its own private copy.

---

## 2. ELF Loading on x86_64

Before TLS can function, the ELF binary itself must be loaded. Understanding this process is essential because TLS initialization is deeply embedded in it.

### 2.1 The `execve()` Path

When a program is launched via `execve()`, the Linux kernel performs several steps:

1. **ELF identification**: The kernel recognizes the `\x7fELF` magic bytes and parses the ELF header.
2. **Program header parsing**: The kernel iterates over the program headers (type `Elf64_Phdr`), looking for segments to load.
3. **PT_LOAD segments**: Each `PT_LOAD` segment is mapped into the process address space via `mmap()`. The segments specify virtual address, file offset, memory size, file size, and permissions (read/write/execute).
4. **PT_INTERP detection**: If a `PT_INTERP` header is present, the kernel reads the null-terminated path it contains — on x86_64 Linux, this is typically `/lib64/ld-linux-x86-64.so.2` (the glibc dynamic linker). The kernel then loads this dynamic linker into memory as well.
5. **Auxiliary vector (auxv)**: The kernel constructs an auxiliary vector on the new process's stack, containing key information like `AT_PHDR` (address of the program headers), `AT_PHNUM` (number of program headers), `AT_ENTRY` (entry point of the executable), `AT_BASE` (base address of the dynamic linker), and `AT_RANDOM` (16 bytes of random data, used for stack canaries).
6. **Control transfer**: Execution begins at the dynamic linker's entry point (not the program's `_start`).

### 2.2 The Dynamic Linker's Job

The dynamic linker (`ld-linux-x86-64.so.2`) is itself a shared object (ELF type `ET_DYN`). Before it can do anything, it must **self-relocate** — it patches its own GOT entries using its load address. After self-bootstrap:

1. **Dependency resolution**: It reads the `DT_NEEDED` entries from the executable's `.dynamic` section, recursively discovering all required shared libraries.
2. **Library search**: Libraries are located using (in order): `DT_RPATH` (deprecated), `LD_LIBRARY_PATH`, `DT_RUNPATH`, the `/etc/ld.so.cache`, and default paths (`/lib`, `/usr/lib`).
3. **Mapping**: Each shared object's `PT_LOAD` segments are mapped into memory at ASLR-randomized addresses.
4. **Symbol resolution and relocation**: The dynamic linker processes the `.rela.dyn` and `.rela.plt` sections. For each relocation entry, it looks up the target symbol in the global symbol table (using hash tables — `DT_GNU_HASH` or `DT_HASH`) and writes the resolved address into the appropriate GOT or data slot.
5. **TLS initialization**: The dynamic linker locates `PT_TLS` segments across all initially loaded modules and sets up the Static TLS Block (see Section 4).
6. **PLT lazy binding**: By default, `R_X86_64_JUMP_SLOT` relocations in the PLT GOT are filled with a trampoline back into the dynamic linker. The first call to a PLT entry triggers resolution. This can be disabled with `LD_BIND_NOW` or the `-z now` linker flag.
7. **Transfer to `_start`**: Finally, control passes to the executable's entry point.

### 2.3 Key ELF Structures for x86_64

The ELF64 header is at offset 0 of the file:

```
Offset  Size  Field
0x00    4     e_ident[EI_MAG0..3] = 0x7f 'E' 'L' 'F'
0x04    1     e_ident[EI_CLASS]   = 2 (64-bit)
0x05    1     e_ident[EI_DATA]    = 1 (little-endian)
0x10    2     e_type              = ET_EXEC(2) or ET_DYN(3)
0x12    2     e_machine           = EM_X86_64 (62)
0x18    8     e_entry             = virtual entry point
0x20    8     e_phoff             = offset to program headers
0x28    8     e_shoff             = offset to section headers
0x36    2     e_phentsize         = 56 (sizeof Elf64_Phdr)
0x38    2     e_phnum             = number of program headers
```

Relevant program header types:

| Type       | Value | Purpose                                |
|------------|-------|----------------------------------------|
| `PT_LOAD`  | 1     | Loadable segment (code, data)          |
| `PT_DYNAMIC`| 2    | Dynamic linking information            |
| `PT_INTERP`| 3     | Path to dynamic linker                 |
| `PT_TLS`   | 7     | Thread-local storage template          |
| `PT_GNU_RELRO` | 0x6474e552 | Read-only after relocation      |

---

## 3. TLS Fundamentals

### 3.1 ELF Sections for TLS

TLS variables live in two special sections:

- **`.tdata`**: Initialized thread-local variables (equivalent to `.data` for regular variables). Has both `SHF_TLS` and `SHF_WRITE` flags.
- **`.tbss`**: Zero-initialized thread-local variables (equivalent to `.bss`). Has `SHF_TLS`, `SHF_WRITE`, and `SHF_ALLOC` but occupies no file space.

The static linker combines these into a single `PT_TLS` program header with:

| Field       | Meaning                                          |
|-------------|--------------------------------------------------|
| `p_offset`  | File offset of the TLS initialization image      |
| `p_vaddr`   | Virtual address (used for alignment calculations)|
| `p_filesz`  | Size of `.tdata` (initialized portion)           |
| `p_memsz`   | Size of `.tdata` + `.tbss` (total TLS block)     |
| `p_align`   | Alignment requirement                            |

The virtual address of the `PT_TLS` segment is not directly meaningful — the segment is not loaded at a fixed address. Instead, the initialization image is copied to a per-thread allocation decided at runtime.

### 3.2 TLS Symbols

Symbols referencing TLS variables have type `STT_TLS` (type 6) in the ELF symbol table. Their `st_value` field contains the offset within the module's TLS initialization image, not a virtual address. Only TLS relocations may reference `STT_TLS` symbols, and TLS relocations may only reference `STT_TLS` symbols.

### 3.3 Modules and Module IDs

In the TLS context, a *module* is an executable or shared library. Each loaded module that has a `PT_TLS` segment is assigned a **module ID** at load time. The main executable always has module ID 1. Shared libraries receive incrementing IDs as they are loaded. The combination of (module ID, offset within TLS block) uniquely identifies any TLS variable in the process.

---

## 4. TLS Data Structure Layout (Variant II)

x86_64 uses **Variant II** of the TLS data structure layout (inherited from IA-32 for historical compatibility). The critical distinction from Variant I (used on ARM, RISC-V, etc.) is that **TLS blocks grow downward from the thread pointer**, while the Thread Control Block (TCB) sits at the thread pointer address.

### 4.1 Memory Layout

For thread `t`, the layout is:

```
Low address                                              High address
  ┌──────────────┬──────────────┬─────────┬───────────────────┐
  │  TLS Block 2 │  TLS Block 1 │ (pad)   │ TCB (struct pthread)│
  │  (libfoo.so) │ (executable) │         │ = Thread Pointer    │
  └──────────────┴──────────────┴─────────┴───────────────────┘
                                           ▲
                                           │
                                       FS register
                                     (Thread Pointer)
```

Key properties:

- **FS register points to the TCB**. On x86_64, the `%fs` segment register base holds the thread pointer. The value at `%fs:0` is a self-pointer (the TCB's first field points to itself).
- **Static TLS blocks are at negative offsets from FS**. The executable's TLS block is immediately below the TCB; shared library TLS blocks follow below that.
- **DTV (Dynamic Thread Vector)** is at `%fs:8` (`dtv` field of `struct pthread`). It is an array of pointers, indexed by module ID, where each entry points to the start of that module's TLS block.

### 4.2 The Thread Control Block (TCB)

On x86_64 glibc, the TCB is `struct pthread` (also called the "thread descriptor"). Its layout begins with `tcbhead_t`, defined with fields including:

```
Offset   Field                Description
0x00     tcb (self)           Self-pointer (== thread pointer)
0x08     dtv                  Pointer to Dynamic Thread Vector
0x10     self                 Pointer to struct pthread
0x18     multiple_threads     Multi-thread flag
0x1c     gscope_flag          Global scope lock flag
0x20     sysinfo              vDSO entry point
0x28     stack_guard          Stack canary value (%fs:0x28)
0x30     pointer_guard        Pointer encryption guard
```

The self-pointer at `%fs:0` is an ABI requirement: code can load the thread pointer with `mov %fs:0, %rax` without needing to know the actual FS base address.

The **stack canary** at `%fs:0x28` is used by `-fstack-protector`. GCC/Clang emit code that loads the canary from this fixed TLS offset.

### 4.3 TP Offset Calculation (Variant II)

For the main executable's TLS block (module 1), the thread-pointer offset of a variable is calculated as:

```
TPOFF(var) = -(TLS_block_size - var_offset_in_block)
```

Or equivalently, as described in the Fuchsia documentation: on x86_64, `TPOFF_a == -<a>` where `<a>` is the offset of variable `a` measured from the **end** of the main executable's TLS segment. Since the TLS block is placed immediately before the TCB (which is at the thread pointer), a variable at the beginning of the block has the most negative offset.

For a static TLS variable `x` in the executable, accessing it at runtime is:

```asm
mov %fs:TPOFF(x), %eax     ; direct FS-relative access
```

This is the most efficient access pattern — a single instruction with a fixed offset known at link time.

### 4.4 The Dynamic Thread Vector (DTV)

The DTV is an array allocated per-thread. Entry 0 holds a generation counter (used to detect when the DTV needs to grow because new modules were `dlopen`'d). Entries 1..N hold pointers to TLS blocks:

```
DTV[0].counter = generation count
DTV[1].pointer = address of module 1's TLS block (executable)
DTV[2].pointer = address of module 2's TLS block (first .so)
...
DTV[m].pointer = address of module m's TLS block
```

For modules in the static TLS set (present at startup), the DTV entries point into the pre-allocated static TLS region below the TCB. For dynamically loaded modules (`dlopen`), TLS blocks are allocated lazily, often on first access.

---

## 5. The Four TLS Access Models

The ELF TLS ABI defines four access models, ordered from most general (slowest) to most restrictive (fastest). The choice depends on what the compiler and linker know about where the variable is defined and how the code will be linked.

### 5.1 General Dynamic (GD)

**When used**: Default for position-independent code (`-fpic`/`-fPIC`). Works for any TLS variable, whether defined in the same module, another shared library, or a `dlopen`'d library. This is the most general model.

**Mechanism**: Neither the module ID nor the variable offset within the module's TLS block is known at link time. The generated code calls `__tls_get_addr()` with a pointer to a GOT entry containing a `tls_index` structure (module ID + DTPOFF).

**x86_64 code sequence**:

```asm
.byte 0x66                              # data16 prefix
leaq    x@TLSGD(%rip), %rdi            # R_X86_64_TLSGD
.word   0x6666                          # two data16 prefixes
rex64
call    __tls_get_addr@PLT              # R_X86_64_PLT32
# %rax now holds the absolute address of 'x' for this thread
```

The `data16` and `rex64` prefixes are deliberate padding. They make the `leaq`+`call` sequence exactly **16 bytes**, which is critical for linker relaxation — the linker may need to replace this entire 16-byte sequence with a shorter instruction sequence for a more efficient model.

**GOT entries**: Two contiguous 8-byte entries:
- `GOT[n]`: filled by `R_X86_64_DTPMOD64` (module ID, resolved at runtime)
- `GOT[n+1]`: filled by `R_X86_64_DTPOFF64` (offset within module's TLS block)

### 5.2 Local Dynamic (LD)

**When used**: When the compiler knows the variable is defined in the same module (e.g., `static __thread` or hidden visibility), but the module ID is still unknown (because we're building a shared library).

**Optimization over GD**: Multiple local TLS variables within the same module can share a single call to `__tls_get_addr` to get the module's TLS block base address. Individual variable offsets (known at link time) are then added.

**x86_64 code sequence**:

```asm
# Get module base (one call, shared across variables)
leaq    x@TLSLD(%rip), %rdi            # R_X86_64_TLSLD
call    __tls_get_addr@PLT              # R_X86_64_PLT32
# %rax = base of this module's TLS block

# Access individual variables with known offsets
movl    x@DTPOFF(%rax), %edx           # R_X86_64_DTPOFF32
addl    y@DTPOFF(%rax), %edx           # R_X86_64_DTPOFF32
```

**GOT entries**: Two entries, but the DTPOFF entry is zeroed (the DTPMOD is still needed):
- `GOT[n]`: `R_X86_64_DTPMOD64` (module ID)
- `GOT[n+1]`: 0 (the offset is encoded in the code, not the GOT)

The key advantage: if a function accesses 10 `static __thread` variables, only one `__tls_get_addr` call is needed instead of 10.

### 5.3 Initial Exec (IE)

**When used**: When the variable is known to be in the static TLS set (i.e., in a module loaded at program startup, not `dlopen`'d). The thread-pointer offset is stored in the GOT and resolved at load time by the dynamic linker.

**Mechanism**: A GOT entry holds the TP-relative offset of the variable. The code loads this offset from the GOT and adds it to the FS base.

**x86_64 code sequence**:

```asm
movq    x@GOTTPOFF(%rip), %rax         # R_X86_64_GOTTPOFF
                                         # Load TP offset from GOT
movl    %fs:(%rax), %eax               # Add FS base, load value
```

Or equivalently, to get the address:

```asm
movq    x@GOTTPOFF(%rip), %rax         # Load TP offset from GOT
addq    %fs:0, %rax                    # Add thread pointer
# %rax = address of x
```

**GOT entry**: A single 8-byte entry with `R_X86_64_TPOFF64`, filled by the dynamic linker with the signed offset from TP to the variable.

This is significantly faster than GD/LD: no function call, just a GOT load and an FS-relative access. However, it requires a GOT entry (one memory indirection) because the offset isn't known until load time.

### 5.4 Local Exec (LE)

**When used**: When the variable is defined in the main executable and the code is in the executable (not a shared library). Both the module ID (always 1) and the TP offset are known at static link time.

**Mechanism**: The TP offset is embedded directly in the instruction as an immediate or displacement. No GOT access, no function call.

**x86_64 code sequence**:

```asm
movl    %fs:x@TPOFF, %eax              # R_X86_64_TPOFF32
                                         # Direct FS-relative access
```

Or to get the address:

```asm
movq    %fs:0, %rax                    # Load thread pointer
leaq    x@TPOFF(%rax), %rax            # Add known offset
```

**No GOT entry needed**. The offset is a 32-bit signed immediate patched by the static linker at link time. This is the fastest possible TLS access — a single instruction with a fixed displacement.

### 5.5 Model Selection Summary

| Model | Module ID known? | Offset known? | GOT entries | Function call? | Use case |
|-------|:----------------:|:-------------:|:-----------:|:--------------:|----------|
| GD    | No               | No            | 2           | Yes            | Any TLS variable from PIC code |
| LD    | No               | Yes           | 2 (shared)  | Yes (once)     | Module-local vars in PIC code |
| IE    | N/A              | At load time  | 1           | No             | Static TLS set, known at link time |
| LE    | Yes (== 1)       | At link time  | 0           | No             | Executable-defined vars in exec |

Compiler flags and model selection:
- `-ftls-model=global-dynamic` — forces GD (default for `-fpic`)
- `-ftls-model=local-dynamic` — forces LD
- `-ftls-model=initial-exec` — forces IE
- `-ftls-model=local-exec` — forces LE

---

## 6. TLS Relocation Types on x86_64

### 6.1 Initial Relocations (in `.rela.dyn`)

These are processed by the dynamic linker at load time:

| Relocation | Value | Description |
|------------|-------|-------------|
| `R_X86_64_DTPMOD64` | 16 | Module ID for GD/LD GOT entries |
| `R_X86_64_DTPOFF64` | 17 | DTPOFF for GD GOT entries |
| `R_X86_64_TPOFF64`  | 18 | TP-relative offset for IE GOT entries |

### 6.2 Static Relocations (in `.rela.text` / `.rela.*`)

These are processed by the static linker at link time:

| Relocation | Value | Model | Description |
|------------|-------|-------|-------------|
| `R_X86_64_TLSGD`     | 19 | GD | PC-relative offset to TLSGD GOT entry |
| `R_X86_64_TLSLD`     | 20 | LD | PC-relative offset to TLSLD GOT entry |
| `R_X86_64_DTPOFF32`  | 21 | LD | 32-bit offset within module's TLS block |
| `R_X86_64_GOTTPOFF`  | 22 | IE | PC-relative offset to GOTTPOFF GOT entry |
| `R_X86_64_TPOFF32`   | 23 | LE | 32-bit signed TP-relative offset |

### 6.3 TLSDESC Relocations

The TLSDESC mechanism (see Section 9) uses additional relocation types:

| Relocation | Value | Description |
|------------|-------|-------------|
| `R_X86_64_GOTPC32_TLSDESC` | 34 | GOT offset for TLS descriptor |
| `R_X86_64_TLSDESC_CALL`    | 35 | Marker for call through TLS descriptor |
| `R_X86_64_TLSDESC`         | 36 | TLS descriptor (dynamic relocation) |

---

## 7. Linker TLS Optimization (Relaxation)

A powerful feature of the ELF TLS system is that the **static linker can transform code sequences** from a more general model to a more efficient one when it has enough information. This is called TLS relaxation or TLS optimization.

### 7.1 Why Relaxation Works

The compiler emits code with specific relocation tags (e.g., `R_X86_64_TLSGD`) that not only request address patching but also identify which TLS model is being used. The linker recognizes these tagged sequences and, when it determines a more efficient model is valid, rewrites the instructions in-place.

On x86_64, the GD code sequence is deliberately padded to 16 bytes with `data16` and `rex64` prefixes. This ensures the linker has enough space to substitute alternative instruction sequences without changing code size or shifting any addresses.

### 7.2 Possible Transitions

The linker can perform these model transitions depending on the output type:

```
                   ┌──────────┐
                   │    GD    │
                   └────┬─────┘
                 ┌──────┼──────┐
                 ▼      ▼      ▼
              ┌────┐ ┌────┐ ┌────┐
              │ LD │ │ IE │ │ LE │
              └──┬─┘ └──┬─┘ └────┘
                 │      ▼
                 │   ┌────┐
                 └──►│ LE │
                     └────┘
```

| Transition | When applied |
|------------|-------------|
| GD → IE | Linking into executable; variable defined in shared lib present at startup |
| GD → LE | Linking into executable; variable defined in the executable itself |
| LD → LE | Linking into executable; all local TLS offsets are TP-relative |
| IE → LE | Linking into executable; variable defined in the executable itself |

When building a shared library, the linker generally cannot relax because it doesn't know the final link context. However, GD→LD is possible if the variable has hidden/protected visibility.

### 7.3 GD → IE Relaxation on x86_64

The 16-byte GD sequence:

```asm
.byte 0x66
leaq    x@TLSGD(%rip), %rdi        # 4 bytes
.word 0x6666
rex64
call    __tls_get_addr@PLT          # 5 bytes (+ 3 prefix bytes)
```

Is transformed to:

```asm
movq    %fs:0, %rax                 # 9 bytes
addq    x@GOTTPOFF(%rip), %rax      # 7 bytes
```

Total: still 16 bytes. The `__tls_get_addr` call is eliminated, replaced by a direct GOT load. The GOT entry changes from a two-slot `tls_index` to a single `R_X86_64_TPOFF64`.

### 7.4 GD → LE Relaxation on x86_64

When the variable is in the executable itself:

```asm
movq    %fs:0, %rax                 # 9 bytes
leaq    x@TPOFF(%rax), %rax         # 7 bytes
```

Total: 16 bytes. No GOT access at all — the TP offset is an immediate in the `leaq`.

### 7.5 IE → LE Relaxation

The IE sequence:

```asm
movq    x@GOTTPOFF(%rip), %rax      # 7 bytes
```

Becomes:

```asm
movq    $x@TPOFF, %rax              # 7 bytes
```

The GOT load is replaced by an immediate move. The instruction encoding stays the same size.

---

## 8. Runtime TLS Resolution: `__tls_get_addr`

The `__tls_get_addr` function is provided by the dynamic linker (or libc) and is the runtime backbone of the GD and LD access models.

### 8.1 Interface

```c
typedef struct {
    unsigned long int ti_module;    // Module ID
    unsigned long int ti_offset;    // Offset within module's TLS block
} tls_index;

extern void *__tls_get_addr(tls_index *ti);
```

The function takes a pointer to a `tls_index` (which lives in the GOT) and returns the absolute address of the TLS variable for the calling thread.

### 8.2 Fast Path

In glibc's optimized implementation, `__tls_get_addr` first checks whether the calling thread's DTV is up-to-date (DTV generation matches the global generation) and whether `DTV[ti_module]` is already populated. If so, it simply computes `DTV[ti_module].pointer + ti_offset` and returns — this is the fast path.

### 8.3 Slow Path

If the DTV is stale (a new module was loaded via `dlopen` since this thread last updated its DTV) or the TLS block hasn't been allocated yet, `__tls_get_addr` falls into the slow path:

1. **Resize the DTV** if needed (if a new module's ID exceeds the current DTV capacity).
2. **Allocate the TLS block** for the requested module on the heap.
3. **Initialize** the block from the module's TLS initialization image (`.tdata` contents + zero-fill for `.tbss`).
4. **Store** the pointer in `DTV[ti_module]`.
5. Return `DTV[ti_module].pointer + ti_offset`.

The slow path involves a global lock (`dl_load_lock` or similar), memory allocation, and `memcpy`, making it significantly more expensive. This is why the IE and LE models, which avoid `__tls_get_addr` entirely, are strongly preferred when applicable.

---

## 9. TLSDESC: An Alternative Calling Convention

The traditional GD model requires a call to `__tls_get_addr` which is both expensive (function call overhead, parameter marshalling) and inflexible. The **TLS Descriptor (TLSDESC)** mechanism, proposed by Alexandre Oliva, provides an alternative.

### 9.1 Concept

Instead of a `tls_index` structure in the GOT, a TLS descriptor is a two-word entry containing a function pointer and an argument. The code loads the descriptor and calls through the function pointer:

```asm
leaq    x@TLSDESC(%rip), %rax       # R_X86_64_GOTPC32_TLSDESC
call    *x@TLSCALL(%rax)             # R_X86_64_TLSDESC_CALL
# %rax now contains the TP offset
# use with %fs:(%rax) or add %fs:0 to get the address
```

### 9.2 Resolution Functions

The dynamic linker resolves `R_X86_64_TLSDESC` relocations by filling in the descriptor with one of several possible resolver functions:

- **`__tlsdesc_static`**: For variables in the static TLS set. The second word holds the TP offset directly. The resolver simply loads and returns this offset. Extremely fast — comparable to IE.
- **`__tlsdesc_dynamic`**: For dynamically loaded modules. The resolver effectively calls into `__tls_get_addr` logic.
- **Direct LE value**: When relaxed to LE, the call becomes a no-op and `%rax` already contains the offset.

### 9.3 Advantages

TLSDESC is more efficient than traditional GD because:
- The fast case (static TLS) avoids the full `__tls_get_addr` calling convention overhead.
- The function pointer call can be inlined or resolved to a tiny trampoline.
- It supports lazy resolution of TLS descriptors (though glibc now eagerly resolves them due to data race concerns).

GCC uses TLSDESC when compiling with `-mtls-dialect=gnu2`. LLVM/Clang also supports it.

---

## 10. Kernel and libc Bootstrap

### 10.1 FS Register Setup

On x86_64, the `%fs` segment register base is stored in the `MSR_FS_BASE` Model-Specific Register (MSR address `0xC0000100`). User-space programs cannot directly modify FS; instead:

- The `arch_prctl(ARCH_SET_FS, addr)` system call sets FS base for the calling thread.
- Newer CPUs support the `WRFSBASE` instruction (enabled by the kernel after checking `CPUID` for FSGSBASE support), which allows user-space to set FS directly without a syscall.
- During context switches, the kernel saves and restores each thread's FS base from the task structure.

### 10.2 Main Thread TLS Bootstrap

The kernel does **not** set up TLS for the main thread. This is the responsibility of the C library (specifically the dynamic linker, which is part of glibc). During startup:

1. The dynamic linker parses `PT_TLS` segments from the executable and all initially loaded shared objects.
2. It computes the total static TLS size (sum of all modules' `p_memsz`, with alignment padding).
3. It allocates memory for the TCB (at the high end) and the static TLS blocks (growing downward from the TCB).
4. The `.tdata` initialization images are copied into the appropriate slots; `.tbss` regions are zero-filled.
5. The DTV is allocated with entries pointing to each module's TLS block.
6. The TCB self-pointer is set.
7. `arch_prctl(ARCH_SET_FS, tcb_address)` is called to point FS at the TCB.

### 10.3 New Thread TLS Setup

When `pthread_create` is called:

1. glibc's `allocate_stack()` function allocates a memory region for the new thread's stack.
2. **The TCB and static TLS are placed at the top of the stack allocation** (for Variant II). This avoids a separate heap allocation — the TLS is embedded in the stack memory.
3. The TLS initialization images are copied from the master copies.
4. A new DTV is allocated.
5. The `clone()` syscall is invoked with the `CLONE_SETTLS` flag, passing the new TCB address. The kernel sets the new thread's FS base.

### 10.4 `dlopen` and Dynamic TLS

When a shared library with TLS is loaded via `dlopen`:

1. The new module is assigned a module ID.
2. The global generation counter is incremented.
3. The new module's TLS block is **not** immediately allocated for existing threads.
4. When a thread first accesses TLS from the new module (via `__tls_get_addr`), the slow path detects the stale DTV, resizes it, allocates the TLS block, and initializes it.

This lazy allocation means `dlopen` itself is fast, but the first TLS access per thread per module pays the allocation cost.

---

## 11. Practical Examples

### 11.1 Examining TLS in a Binary

```bash
# View PT_TLS segment
$ readelf -l a.out | grep TLS
  TLS   0x0000000000001000 0x0000000000201000 ...

# View TLS symbols
$ readelf -s a.out | grep TLS
  12: 0000000000000000     4 TLS     GLOBAL DEFAULT   16 my_var

# View TLS relocations
$ readelf -r libfoo.so | grep TLS
0000000000003ff8  R_X86_64_DTPMOD64  0000000000000000 my_var + 0
0000000000004000  R_X86_64_DTPOFF64  0000000000000000 my_var + 0

# Check TLS model in disassembly
$ objdump -d -r a.out | grep -A2 tls
```

### 11.2 Compiler Output for Each Model

Given `extern __thread int x;`, compiling with different models:

**GD** (`-fpic -ftls-model=global-dynamic`):
```asm
data16 leaq x@TLSGD(%rip), %rdi
data16 data16 rex64 call __tls_get_addr@PLT
```

**IE** (`-ftls-model=initial-exec`):
```asm
movq x@GOTTPOFF(%rip), %rax
movl %fs:(%rax), %eax
```

**LE** (`-ftls-model=local-exec`):
```asm
movl %fs:x@TPOFF, %eax
```

### 11.3 Verifying Relaxation

```bash
# Compile with GD model
$ gcc -fpic -ftls-model=global-dynamic -c foo.c -o foo.o

# Link into executable (linker should relax GD→LE if 'x' is defined here)
$ gcc foo.o -o foo

# Check: should see %fs:TPOFF instead of __tls_get_addr
$ objdump -d foo | grep -A3 'tls'
```

---

## 12. References

1. Ulrich Drepper, "ELF Handling For Thread-Local Storage" (2003) — The canonical specification. Available at uclibc.org/docs/tls.pdf
2. System V ABI, AMD64 Architecture Processor Supplement — Defines x86_64 ELF relocation types and conventions.
3. MaskRay (Fangrui Song), "All about thread-local storage" (2021) — Comprehensive modern treatment at maskray.me
4. chao-tic, "A Deep dive into (implicit) Thread Local Storage" (2018) — Detailed walkthrough of glibc internals at chao-tic.github.io
5. Stafford Horne, "Thread Local Storage" (2020) — Excellent comparison of TLS models with disassembly at stffrdhrn.github.io
6. Fuchsia OS, "Thread Local Storage" documentation — Clear explanation of Variant I vs II and DTV mechanics at fuchsia.dev
7. OSDev Wiki, "Thread Local Storage" — Practical OS developer perspective at wiki.osdev.org
8. Oracle Linker and Libraries Guide, Chapter 8/14 — Detailed code sequences and relocation descriptions for Solaris/x86
9. Alexandre Oliva, "RFC: TLS Descriptors for x86" — The TLSDESC proposal at fsfla.org
