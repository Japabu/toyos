# How to Write an Operating System

A step-by-step guide to building an OS from scratch. By the end you will have a UEFI bootloader, a kernel with a text console and keyboard input, and an NVMe storage driver that lets you create, edit, and persist files across reboots.

Target: x86_64 with UEFI. Pseudocode throughout — implement in whatever language you like.

## Prerequisites

- QEMU (with `q35` machine support)
- OVMF firmware (UEFI for QEMU)
- A language that can emit UEFI PE/COFF executables and freestanding x86_64 ELF binaries

---

## Table of Contents

1. [A Minimal UEFI Application](#1-a-minimal-uefi-application)
2. [A Bootable Disk Image](#2-a-bootable-disk-image)
3. [An Empty Kernel](#3-an-empty-kernel)
4. [Loading and Jumping to the Kernel](#4-loading-and-jumping-to-the-kernel)
5. [Serial Output — Hello from Kernel](#5-serial-output--hello-from-kernel)
6. [ELF Relocations — Why Your Strings Crash](#6-elf-relocations--why-your-strings-crash)
7. [A Memory Allocator](#7-a-memory-allocator)
8. [Pixels on Screen — The Framebuffer](#8-pixels-on-screen--the-framebuffer)
9. [A Text Console](#9-a-text-console)
10. [Replacing the GDT](#10-replacing-the-gdt)
11. [Interrupts and the Keyboard](#11-interrupts-and-the-keyboard)
12. [ACPI — Shutdown and Hardware Discovery](#12-acpi--shutdown-and-hardware-discovery)
13. [PCI Device Enumeration](#13-pci-device-enumeration)
14. [NVMe Storage Driver](#14-nvme-storage-driver)
15. [A Filesystem](#15-a-filesystem)
16. [An Interactive Shell](#16-an-interactive-shell)

---

## 1. A Minimal UEFI Application

A UEFI application is a PE/COFF executable that the firmware loads and runs. The firmware gives you a `SystemTable` — a struct of function pointers for console I/O, memory allocation, file access, and more. You're in 64-bit mode with flat memory from the start. No real mode, no BIOS interrupts.

### Goal

Print "Hello" to the UEFI console and hang.

### Entry point

```
function efi_main(image_handle, system_table):
    system_table.console_out.output_string("Hello from UEFI!\n")
    loop forever
```

Your toolchain must produce a PE/COFF binary with the UEFI subsystem. The firmware finds and calls your entry point.

### How to test it manually

Copy the `.efi` binary to a FAT32-formatted USB image at `/EFI/BOOT/BOOTx64.EFI`. Boot QEMU with OVMF:

```
qemu-system-x86_64 \
  -drive if=pflash,format=raw,file=OVMF_CODE.fd,readonly=on \
  -drive if=pflash,format=raw,file=OVMF_VARS.fd,readonly=on \
  -drive format=raw,file=your_image.img
```

You should see "Hello from UEFI!" on the emulated display.

---

## 2. A Bootable Disk Image

Manually creating FAT32 images and GPT partition tables is tedious. Automate it.

### Disk layout

Modern UEFI firmware expects a GPT disk with an EFI System Partition (ESP) formatted as FAT32:

```
LBA 0:        Protective MBR
LBA 1-33:     GPT header + partition entries
Partition 1:  EFI System Partition (FAT32)
                /EFI/BOOT/BOOTx64.EFI    ← your bootloader
End:          Backup GPT header
```

### Build script pseudocode

```
function build_disk_image():
    compile bootloader → bootloader.efi

    // Create FAT32 volume (minimum ~34 MB for FAT32 cluster requirements)
    fat_volume = allocate(34 * 1024 * 1024)
    format_fat32(fat_volume)
    write_file(fat_volume, "/EFI/BOOT/BOOTx64.EFI", bootloader_bytes)

    // Wrap in GPT
    disk = allocate(fat_volume.size + 100KB)  // GPT overhead
    write_protective_mbr(disk)
    write_gpt_header(disk)
    add_partition(disk, type=EFI_SYSTEM, data=fat_volume)

    write_to_file("bootable.img", disk)
```

### QEMU launch

```
qemu-system-x86_64 \
  -machine q35 -cpu qemu64 -m 1G \
  -drive if=pflash,format=raw,file=OVMF_CODE.fd,readonly=on \
  -drive if=pflash,format=raw,file=OVMF_VARS.fd,readonly=on \
  -device nec-usb-xhci,id=xhci \
  -drive if=none,id=stick,format=raw,file=bootable.img \
  -device usb-storage,bus=xhci.0,drive=stick,bootindex=0 \
  -serial stdio
```

From this point on, changing any source file triggers a rebuild of the entire image automatically. You should see your UEFI "Hello" message.

---

## 3. An Empty Kernel

The kernel is a separate binary from the bootloader. It's a freestanding ELF — no standard library, no libc, no OS beneath it.

### Requirements

- Target: `x86_64`, no OS, no standard library
- Format: ELF (not PE/COFF — the bootloader will parse it)
- Entry point: a function with a known calling convention
- Must not return (there's nowhere to return to)

### Minimal kernel

```
// No standard library. No main(). No runtime.

function _start():
    loop forever    // just hang
```

Configure your build so this produces a static ELF binary with `_start` as the entry point. No dynamic linking, no libc startup code. The resulting file should be a few kilobytes at most.

### Get it onto the disk image

Update your build script:

```
function build_disk_image():
    compile bootloader → bootloader.efi
    compile kernel     → kernel.elf            // ← new

    fat_volume = allocate(34MB)
    format_fat32(fat_volume)
    write_file(fat_volume, "/EFI/BOOT/BOOTx64.EFI", bootloader_bytes)
    write_file(fat_volume, "/toyos/kernel.elf", kernel_bytes)   // ← new
    // ... GPT wrapping same as before
```

The kernel sits on the FAT32 partition. The bootloader will load it next.

---

## 4. Loading and Jumping to the Kernel

The bootloader's real job: load the kernel ELF into memory, set up a stack, and jump.

### Step 1: Read the kernel file

UEFI gives you a filesystem protocol. Use it to open and read `\toyos\kernel.elf` into a buffer:

```
function load_file(system_table, path) -> bytes:
    fs = system_table.boot_services.get_filesystem(image_handle)
    file = fs.open_volume().open(path, READ)
    size = file.get_info().file_size
    buffer = allocate(size)
    file.read(buffer)
    return buffer
```

### Step 2: Parse the ELF

An ELF binary contains *program headers* that describe memory segments:

```
function load_elf(elf_bytes) -> loaded_kernel:
    parse elf_bytes → header, program_headers

    // Find how much memory the kernel needs
    mem_size = 0
    for segment in program_headers:
        if segment.type == PT_LOAD:
            mem_size = max(mem_size, segment.vaddr + segment.memsize)

    // Add 8 MB for the kernel stack
    stack_size = 8 * 1024 * 1024
    mem_size += stack_size

    // Allocate and load
    memory = allocate_zeroed(mem_size)
    for segment in program_headers:
        if segment.type == PT_LOAD:
            copy(elf_bytes[segment.offset .. segment.filesz],
                 memory[segment.vaddr ..])

    return {
        memory,
        entry_offset: header.entry_point,
        stack_offset: mem_size - stack_size,
        stack_size
    }
```

### Step 3: Jump to the kernel

Before jumping, you must exit UEFI boot services — after this, all UEFI runtime calls are gone and you own the machine. Pass the kernel the information it needs.

For now, the kernel doesn't need much. Start with a minimal struct — you'll grow it later:

```
struct KernelArgs:
    // We'll add fields here as we need them
    // (empty for now — kernel just loops)
```

```
function start_kernel(kernel, system_table):
    // Exit boot services — point of no return
    system_table.exit_boot_services()

    // Set up the stack pointer and jump
    entry = kernel.memory + kernel.entry_offset
    stack = kernel.memory + kernel.stack_offset + kernel.stack_size

    set_stack_pointer(stack)
    call entry(kernel_args)    // sysv64 calling convention
```

After `exit_boot_services()`, you must not touch any UEFI data structures — their memory may have been reclaimed. Use `mem.forget()` (or equivalent) on any bootloader-allocated buffers that the kernel still needs, so they aren't freed.

At this point: QEMU boots, the bootloader loads the kernel, and the kernel hangs. No output yet. But the CPU is running your code.

---

## 5. Serial Output — Hello from Kernel

The kernel is running but has no way to prove it. The simplest output device is the serial port — it requires no complex setup, and QEMU's `-serial stdio` flag pipes it directly to your terminal.

### The hardware

The Intel 8250/16550 UART lives at I/O port `0x3F8` (COM1). To write a byte, you need two x86 instructions: `in` (read from port) and `out` (write to port).

```
function outb(port, value):
    asm("out dx, al", dx=port, al=value)

function inb(port) -> byte:
    asm("in al, dx", dx=port) -> al
```

### Initialization

```
PORT = 0x3F8

function init_serial():
    outb(PORT + 1, 0x00)    // disable interrupts
    outb(PORT + 3, 0x80)    // enable DLAB (baud rate divisor access)
    outb(PORT + 0, 0x03)    // baud divisor low: 38400 baud
    outb(PORT + 1, 0x00)    // baud divisor high
    outb(PORT + 3, 0x03)    // 8 data bits, no parity, 1 stop bit
    outb(PORT + 2, 0xC7)    // enable FIFO, 14-byte threshold
    outb(PORT + 4, 0x0B)    // RTS/DSR set, IRQs enabled

    // Self-test: loopback mode
    outb(PORT + 4, 0x1E)    // enable loopback
    outb(PORT + 0, 0xAE)    // send test byte
    assert inb(PORT) == 0xAE
    outb(PORT + 4, 0x0F)    // normal operation
```

### Sending a character

```
function serial_putchar(ch):
    while (inb(PORT + 5) & 0x20) == 0:   // wait for transmit buffer empty
        spin
    outb(PORT, ch)

function serial_print(string):
    for ch in string:
        serial_putchar(ch)
    serial_putchar('\n')
```

### Update the kernel

```
function _start():
    init_serial()
    serial_print("Hello from Kernel!")
    loop forever
```

Run it. Your host terminal (via QEMU's `-serial stdio`) should print `Hello from Kernel!`. You now have debug output. Every future step will use this to verify progress.

---

## 6. ELF Relocations — Why Your Strings Crash

At some point — maybe now, maybe after a seemingly unrelated change — your kernel will crash or print garbage when you use string constants. This section explains why and how to fix it.

### The problem

When you write `serial_print("Hello")`, the compiler stores `"Hello"` in a data section and generates a reference to its address. The address in the ELF is based on a *link-time assumption* about where the binary will be loaded — typically address 0.

But the bootloader loads the kernel at whatever address `allocate()` returns. The string is in memory but the pointer to it still says address `0x1234` when the string is actually at `0x7F801234`.

### The symptoms

- Printing a string outputs garbage, or nothing, or causes a page fault.
- Simple integer operations work fine. Only *symbol references* (strings, function pointers, global variables) break.

### The fix: process relocations in the bootloader

A position-independent ELF contains relocation entries that say "patch this address once you know where the binary is loaded." For a static kernel, the only relocation type you'll see is `R_X86_64_RELATIVE`:

```
meaning: *(load_base + offset) = load_base + addend
```

The bootloader must process these after loading segments:

```
function apply_relocations(elf_bytes, memory):
    for section in section_headers:
        if section.type == SHT_RELA:
            for rela in parse_rela_entries(section):
                if rela.type == R_X86_64_RELATIVE:
                    target = memory + rela.offset
                    *target = memory + rela.addend
                else:
                    panic("unsupported relocation type")
```

Add this to your `load_elf` function, after copying `PT_LOAD` segments. Now string constants, static variables, and function pointers all resolve to correct addresses.

### Why not just load at address 0?

Address 0 is unmapped. You'd need to set up page tables to map the kernel at its expected virtual address. Processing relocations is simpler and lets the kernel live wherever the bootloader puts it.

---

## 7. A Memory Allocator

You need dynamic memory allocation (`alloc`/`free`). The kernel has no `malloc` — build one from the physical memory map.

### Step 1: Get the memory map from the bootloader

UEFI's `GetMemoryMap()` returns a list of physical memory regions with types. Call it right before `exit_boot_services()` and pass the result to the kernel.

**Grow the KernelArgs struct:**

```
struct KernelArgs:
    memory_map_addr     u64     // ← new
    memory_map_size     u64     // ← new
    kernel_memory_addr  u64     // ← new: where the kernel was loaded
    kernel_memory_size  u64     // ← new: total kernel allocation size

struct MemoryMapEntry:
    type   u32     // UEFI memory type
    start  u64     // physical start address
    end    u64     // physical end address
```

Update the bootloader to fill these fields and `mem.forget` the memory map buffer (so it survives `exit_boot_services`).

### Step 2: The bootstrap problem

You need a data structure (a list of regions) to track allocations. But creating a dynamic list requires an allocator. Break the cycle with a small static arena:

```
ARENA: static byte array, 128 KB

function arena_alloc(size, align) -> pointer:
    pos = align_up(arena_position, align)
    arena_position = pos + size
    assert arena_position <= 128KB
    return &ARENA[pos]
```

Use this arena *only* during `init()` to allocate the region-tracking lists. All subsequent allocations go through the real allocator.

### Step 3: Build the region lists

```
function init_allocator(memory_map, kernel_start, kernel_size):
    // Usable memory: filter and merge adjacent regions
    usable = []  // allocated from arena
    for entry in memory_map:
        if entry.type in {LoaderCode, LoaderData, BootServicesCode,
                          BootServicesData, ConventionalMemory}:
            usable.append({start: entry.start, end: entry.end})
    sort usable by start
    merge adjacent/overlapping regions

    // Reserved: memory we must not touch
    reserved = []  // allocated from arena
    reserved.append({0x0000, 0x1000})                           // null page
    reserved.append({kernel_start, kernel_start + kernel_size}) // kernel
    sort reserved by start
```

### Step 4: Allocate

First-fit with alignment:

```
function alloc(size, align) -> pointer:
    for region in usable:
        cursor = region.start
        for reservation in reserved:
            if reservation overlaps [cursor, region.end]:
                // Try the gap before this reservation
                aligned = align_up(cursor, align)
                if aligned + size <= reservation.start:
                    insert {aligned, aligned+size} into reserved (sorted)
                    return aligned as pointer
                cursor = reservation.end
        // Try the gap after all reservations
        aligned = align_up(cursor, align)
        if aligned + size <= region.end:
            insert {aligned, aligned+size} into reserved (sorted)
            return aligned as pointer
    return NULL
```

### Step 5: Deallocate

```
function dealloc(ptr, size):
    remove {ptr, ptr+size} from reserved
    // If this splits a reservation, handle the split
    merge adjacent free regions
```

Register this as the global allocator. You now have `alloc` and `dealloc`. Dynamic data structures (vectors, strings, boxed values) work from this point on.

---

## 8. Pixels on Screen — The Framebuffer

The UEFI Graphics Output Protocol (GOP) gives you a linear framebuffer — a memory region where each 4-byte word is a pixel.

### Step 1: Get framebuffer info from the bootloader

Query GOP before exiting boot services:

```
// In the bootloader:
gop = system_table.boot_services.locate_protocol(GRAPHICS_OUTPUT)
mode = gop.current_mode_info()
fb_addr = gop.framebuffer_address()
fb_size = gop.framebuffer_size()
```

**Grow KernelArgs again:**

```
struct KernelArgs:
    memory_map_addr         u64
    memory_map_size         u64
    kernel_memory_addr      u64
    kernel_memory_size      u64
    framebuffer_addr        u64     // ← new
    framebuffer_size        u64     // ← new
    framebuffer_width       u32     // ← new: pixels
    framebuffer_height      u32     // ← new: pixels
    framebuffer_stride      u32     // ← new: pixels per scanline (≥ width)
    framebuffer_pixel_format u32    // ← new: 0=RGB, 1=BGR
```

### Step 2: put_pixel

Each pixel is 4 bytes. The stride may be larger than the width (padding at end of each row). The byte order depends on the pixel format:

```
function put_pixel(x, y, r, g, b):
    if x >= width or y >= height: return
    offset = (y * stride + x) * 4
    if pixel_format == RGB:
        volatile_write(fb_addr + offset,     r)
        volatile_write(fb_addr + offset + 1, g)
        volatile_write(fb_addr + offset + 2, b)
    else:  // BGR
        volatile_write(fb_addr + offset,     b)
        volatile_write(fb_addr + offset + 1, g)
        volatile_write(fb_addr + offset + 2, r)
```

The framebuffer is MMIO — you must use volatile writes or the compiler may optimize them away.

### Step 3: Clear screen

```
function clear(r, g, b):
    for y in 0..height:
        for x in 0..width:
            put_pixel(x, y, r, g, b)
```

### Step 4: Scroll

When the screen fills up, copy all rows up by N pixels and clear the bottom:

```
function scroll_up(rows):
    bytes_per_row = stride * 4
    memcopy(fb_addr + rows * bytes_per_row,    // src: below the scroll
            fb_addr,                            // dst: top of screen
            (height - rows) * bytes_per_row)    // count
    clear the bottom `rows` pixel rows
```

Test it: call `clear(0, 0, 0)` in the kernel. The screen should turn black.

---

## 9. A Text Console

You have pixels. Now render text.

### The font

An 8x16 bitmap font: 256 glyphs, 16 bytes per glyph (one byte per row, MSB = leftmost pixel). Total: 4096 bytes.

You don't have a filesystem yet, so the font must be embedded directly in the kernel binary. Generate a `font8x16.bin` at build time by rasterizing a monospace TTF at ~14px and thresholding each pixel to 1-bit. Then include the raw bytes as a static array:

```
// At build time: generate font8x16.bin from a TTF
// In the kernel: embed it as a compile-time constant
FONT: static [byte; 4096] = include_bytes("font8x16.bin")
```

Later, when we add a filesystem (Chapter 15), you can load the font from disk instead and remove the embedded copy.

### Drawing a character

```
function draw_char(col, row, char_code):
    glyph = &font[char_code * 16]
    px = col * 8
    py = row * 16
    for y in 0..16:
        byte = glyph[y]
        for x in 0..8:
            if byte & (0x80 >> x):
                put_pixel(px + x, py + y, 255, 255, 255)  // foreground
            else:
                put_pixel(px + x, py + y, 0, 0, 0)        // background
```

### Console state

```
struct Console:
    cursor_col   int
    cursor_row   int
    max_cols     int = framebuffer_width / 8
    max_rows     int = framebuffer_height / 16

function console_putchar(ch):
    if ch == '\n':
        cursor_col = 0
        cursor_row += 1
    else:
        if cursor_col >= max_cols:
            cursor_col = 0
            cursor_row += 1
        draw_char(cursor_col, cursor_row, ch)
        cursor_col += 1

    if cursor_row >= max_rows:
        scroll_up(16)    // scroll by one text row
        cursor_row = max_rows - 1

function console_print(string):
    for ch in string:
        console_putchar(ch)
```

### Backspace

```
function console_backspace():
    if cursor_col > 0:
        cursor_col -= 1
        draw_char(cursor_col, cursor_row, ' ')  // overwrite with space
```

You can now print text to the screen. Route both serial output and console output through a shared `log_print` function so you see messages in both places.

---

## 10. Replacing the GDT

UEFI set up a Global Descriptor Table, but it may live in memory marked as reclaimable. If that memory gets reused, the CPU triple-faults. Replace it with your own GDT in static memory.

### What is the GDT?

The Global Descriptor Table is how x86 defines memory segments. In the old 16/32-bit days, segments controlled base addresses, limits, and permissions for every memory access. In 64-bit long mode, segmentation is mostly vestigial — the CPU ignores base and limit for code and data segments, and all memory is flat. But the GDT still exists because:

1. The CPU **requires** a valid GDT to be loaded at all times.
2. The **CS** (code segment) register still selects between 64-bit and 32-bit mode, and sets the privilege level.
3. The **SS/DS/ES** registers must point to valid data descriptors.

### Segment descriptor format

Each GDT entry is 8 bytes (64 bits). The bit layout is a legacy mess:

```
Bits [63:56]  Base [31:24]       (ignored in 64-bit mode)
Bit  [55]     G  — Granularity   (1 = limit is in 4KB pages)
Bit  [54]     D/B or L           (D=1 for 32-bit, L=1 for 64-bit code)
Bit  [53]     L  — Long mode     (1 = 64-bit code segment)
Bit  [52]     AVL                (available for OS, unused)
Bits [51:48]  Limit [19:16]
Bit  [47]     P  — Present       (1 = segment is valid)
Bits [46:45]  DPL — Privilege    (0 = kernel, 3 = user)
Bit  [44]     S  — Descriptor    (1 = code/data, 0 = system)
Bits [43:40]  Type               (execute, read, write bits)
Bits [39:32]  Base [23:16]
Bits [31:16]  Base [15:0]        (ignored in 64-bit mode)
Bits [15:0]   Limit [15:0]       (ignored in 64-bit mode)
```

### The table

You need exactly three entries:

```
GDT[0] = 0x0000000000000000    // null descriptor (required, CPU demands slot 0 be empty)
GDT[1] = 0x00AF9A000000FFFF    // kernel code segment
GDT[2] = 0x00CF92000000FFFF    // kernel data segment
```

Breaking down `0x00AF9A000000FFFF` (code segment):

```
Byte 6: 0xAF = 1_0_1_0_1111
         G=1      granularity (4KB pages)
         D=0      not 32-bit
         L=1      64-bit code ← this is the key bit
         AVL=0
         Limit[19:16]=0xF

Byte 5: 0x9A = 1_00_1_1010
         P=1       present
         DPL=00    ring 0 (kernel)
         S=1       code/data descriptor
         Type=1010 execute/read

Base and limit fields: all 0xF/0x0 — ignored in long mode.
```

Breaking down `0x00CF92000000FFFF` (data segment):

```
Byte 6: 0xCF = 1_1_0_0_1111
         G=1       granularity
         D=1       32-bit operand size (irrelevant for data in long mode)
         L=0       not a code segment
         AVL=0

Byte 5: 0x92 = 1_00_1_0010
         P=1       present
         DPL=00    ring 0
         S=1       code/data descriptor
         Type=0010 read/write
```

Segment selectors are byte offsets into the GDT. Each entry is 8 bytes:

```
KERNEL_CS = 0x08    // byte offset of GDT[1] — used in CS
KERNEL_DS = 0x10    // byte offset of GDT[2] — used in DS, SS, ES, etc.
```

### Loading it

The GDT must be 16-byte aligned. Point the CPU at it with `lgdt`, then reload every segment register — the old selectors still reference the firmware's GDT.

```
struct GdtPointer (packed):
    limit  u16    // total size of GDT in bytes, minus 1
    base   u64    // physical address of GDT

gdt_ptr = { limit: sizeof(GDT) - 1, base: &GDT }

asm:
    lgdt [gdt_ptr]

    // Reload CS via far return (you cannot mov to CS directly)
    push KERNEL_CS
    lea tmp, [rip + .after]    // address of next instruction
    push tmp
    retfq                      // pops CS:RIP — loads new CS
.after:
    // Reload all data segment registers
    mov ds, KERNEL_DS
    mov es, KERNEL_DS
    mov fs, KERNEL_DS
    mov gs, KERNEL_DS
    mov ss, KERNEL_DS
```

The `retfq` trick: `CS` can only be changed by a far jump or far return. We push the new selector and return address onto the stack, then `retfq` pops both — atomically switching to our new code segment.

This is pure setup code — nothing visibly changes, but without it you'll get random crashes later when the firmware's GDT memory gets reused.

---

## 11. Interrupts and the Keyboard

To receive keyboard input, you need: an Interrupt Descriptor Table (IDT), the 8259 Programmable Interrupt Controller (PIC) configured to route keyboard interrupts, and a handler that reads scancodes.

### Step 1: Set up the IDT

The IDT has 256 entries, each 16 bytes (in 64-bit mode):

```
struct IDTEntry:
    offset_low   u16     // handler address bits [15:0]
    selector     u16     // KERNEL_CS (0x08)
    ist          u8      // 0
    type_attr    u8      // 0x8E = present, interrupt gate, DPL=0
    offset_mid   u16     // handler address bits [31:16]
    offset_high  u32     // handler address bits [63:32]
    reserved     u32     // 0
```

```
function set_idt_entry(vector, handler_address):
    IDT[vector].offset_low  = handler_address & 0xFFFF
    IDT[vector].offset_mid  = (handler_address >> 16) & 0xFFFF
    IDT[vector].offset_high = (handler_address >> 32) & 0xFFFFFFFF
    IDT[vector].selector    = KERNEL_CS
    IDT[vector].type_attr   = 0x8E

idt_ptr = { limit: sizeof(IDT) - 1, base: &IDT }
asm: lidt [idt_ptr]
```

### Step 2: Remap the 8259 PIC

By default, IRQ 0-7 map to CPU vectors 0-7, which collide with CPU exceptions. Remap them to vectors 32+:

```
// ICW1: start initialization
outb(0x20, 0x11)     // master PIC command
outb(0xA0, 0x11)     // slave PIC command

// ICW2: vector offset
outb(0x21, 32)       // master: IRQ 0-7  → vectors 32-39
outb(0xA1, 40)       // slave:  IRQ 8-15 → vectors 40-47

// ICW3: master/slave wiring
outb(0x21, 0x04)     // master: slave on IRQ2
outb(0xA1, 0x02)     // slave: cascade identity 2

// ICW4: 8086 mode
outb(0x21, 0x01)
outb(0xA1, 0x01)

// Mask: enable only IRQ1 (keyboard)
outb(0x21, 0xFD)     // master: bit 1 clear = keyboard unmasked
outb(0xA1, 0xFF)     // slave: all masked
```

Insert `io_wait()` calls (a write to port `0x80`) between each PIC command to give the hardware time to respond.

### Step 3: The keyboard IRQ handler

The keyboard is IRQ1 = vector 33. Write a handler:

```
// This function must be "naked" — no prologue/epilogue
function irq1_entry():
    asm:
        push rax, rcx, rdx, rsi, rdi, r8, r9, r10, r11
        call keyboard_irq_handler
        mov al, 0x20
        out 0x20, al           // send End Of Interrupt to PIC
        pop r11, r10, r9, r8, rdi, rsi, rdx, rcx, rax
        iretq                  // return from interrupt

function keyboard_irq_handler():
    scancode = inb(0x60)       // read from PS/2 data port
    handle_scancode(scancode)
```

Register it: `set_idt_entry(33, &irq1_entry)`. Then `asm: sti` to enable interrupts.

### Step 4: Scancode translation

PS/2 Scan Code Set 1. Make codes (key press) are 0x00-0x7F. Break codes (key release) are the same with bit 7 set.

```
SCANCODE_TABLE[128] = {
    0,0,'1','2','3','4','5','6','7','8','9','0','-','=', 0x08, '\t',
    'q','w','e','r','t','y','u','i','o','p','[',']', '\n', 0,
    'a','s','d','f','g','h','j','k','l',';','\'', '`', 0, '\\',
    'z','x','c','v','b','n','m',',','.','/', 0,0,0, ' ', ...
}

// Separate shifted table with uppercase letters and symbols (!@#$...)
```

Track shift state:

```
shift_held = false

function handle_scancode(code):
    if code == 0x2A or code == 0x36:     // left/right shift press
        shift_held = true; return
    if code == 0xAA or code == 0xB6:     // left/right shift release
        shift_held = false; return
    if code & 0x80:                       // break code — ignore
        return

    table = shift_held ? SHIFTED_TABLE : SCANCODE_TABLE
    ascii = table[code]
    if ascii != 0:
        key_buffer_push(ascii)
```

### Step 5: Ring buffer

Decouple the interrupt handler (producer) from the main loop (consumer):

```
KEY_BUF: array[64] of byte
head = 0
tail = 0

function key_buffer_push(ch):
    next = (head + 1) % 64
    if next != tail:          // buffer not full
        KEY_BUF[head] = ch
        head = next

function try_read_char() -> optional byte:
    if tail == head: return None
    ch = KEY_BUF[tail]
    tail = (tail + 1) % 64
    return Some(ch)
```

Now the main loop can poll `try_read_char()` to get keyboard input.

---

## 12. ACPI — Shutdown and Hardware Discovery

ACPI tables tell you how to control power and where PCIe devices are mapped. The bootloader found the RSDP address from the UEFI config table.

**Grow KernelArgs:**

```
struct KernelArgs:
    memory_map_addr         u64
    memory_map_size         u64
    kernel_memory_addr      u64
    kernel_memory_size      u64
    framebuffer_addr        u64
    framebuffer_size        u64
    framebuffer_width       u32
    framebuffer_height      u32
    framebuffer_stride      u32
    framebuffer_pixel_format u32
    rsdp_addr               u64     // ← new
```

The bootloader finds this by scanning the UEFI configuration table for the ACPI 2.0 GUID.

### Table chain

Every ACPI table starts with a 4-byte ASCII signature and a `u32` length at offset 4.

```
RSDP (address from bootloader):
    offset 24: XSDT address (u64)

XSDT:
    offset 4:  total length (u32)
    offset 36: array of u64 pointers to other tables

Each pointed-to table starts with a signature:
    "MCFG" → PCIe configuration base address
    "FACP" → FADT (power management)
```

### Finding the PCIe ECAM base

```
function find_ecam_base(rsdp_addr) -> optional u64:
    xsdt_addr = read_u64(rsdp_addr + 24)
    length = read_u32(xsdt_addr + 4)
    entry_count = (length - 36) / 8

    for i in 0..entry_count:
        table_addr = read_u64(xsdt_addr + 36 + i * 8)
        signature = read_4_bytes(table_addr)
        if signature == "MCFG":
            return read_u64(table_addr + 44)   // first ECAM entry base address

    return None
```

### Implementing shutdown (ACPI S5)

```
function init_power(rsdp_addr):
    // Find the FADT table (signature "FACP")
    fadt_addr = find_table(rsdp_addr, "FACP")

    // Get PM1a control register port
    pm1a_cnt_blk = read_u32(fadt_addr + 64)     // I/O port for power control

    // Get DSDT address (AML bytecode table)
    dsdt_addr = read_u64(fadt_addr + 140)        // X_DSDT (ACPI 2.0+)
    if dsdt_addr == 0:
        dsdt_addr = read_u32(fadt_addr + 40)     // fallback: 32-bit DSDT

    // Search DSDT for "_S5_" sleep type value
    dsdt_length = read_u32(dsdt_addr + 4)
    for offset in 36 .. dsdt_length - 4:
        if read_4_bytes(dsdt_addr + offset) == "_S5_":
            // Skip past the PackageOp (0x12) and length bytes
            // Extract SLP_TYPa value from the package
            slp_typa = parse_s5_package(dsdt_addr + offset + 4)
            break

    // Save pm1a_cnt_blk and slp_typa for later use

function shutdown():
    value = (slp_typa << 10) | (1 << 13)     // SLP_TYPa + SLP_EN
    outw(pm1a_cnt_blk, value)
    asm: cli
    asm: hlt
```

You now have a `shutdown` command that cleanly powers off the machine.

---

## 13. PCI Device Enumeration

You need PCI to find the NVMe controller. PCIe uses ECAM — Enhanced Configuration Access Mechanism — which maps the entire PCI configuration space into physical memory.

### ECAM address calculation

```
config_address(ecam_base, bus, device, function, register_offset):
    return ecam_base
         + (bus      << 20)
         | (device   << 15)
         | (function << 12)
         | register_offset
```

Read/write with volatile memory operations — this is MMIO.

### Key PCI configuration registers

```
Offset  Size  Register
0x00    u16   Vendor ID (0xFFFF = no device present)
0x02    u16   Device ID
0x04    u16   Command (bit 1 = memory space, bit 2 = bus master)
0x09    u8    Programming Interface
0x0A    u8    Subclass
0x0B    u8    Class Code
0x0E    u8    Header Type (bit 7 = multi-function device)
0x10    u32   BAR0 (Base Address Register 0)
0x14    u32   BAR1 (upper 32 bits if BAR0 is 64-bit)
```

### Enumeration

```
function enumerate_pci(ecam_base):
    for bus in 0..256:
        for device in 0..32:
            vendor = read_u16(ecam_addr(ecam_base, bus, device, 0, 0x00))
            if vendor == 0xFFFF: continue

            log_device(bus, device, 0)

            header_type = read_u8(ecam_addr(ecam_base, bus, device, 0, 0x0E))
            if header_type & 0x80:                // multi-function
                for func in 1..8:
                    vid = read_u16(ecam_addr(ecam_base, bus, device, func, 0x00))
                    if vid != 0xFFFF:
                        log_device(bus, device, func)
```

### Finding a specific device

```
function find_device(ecam_base, target_class, target_subclass):
    // Same enumeration loop, but return (bus, dev, func) when
    // class == target_class and subclass == target_subclass

// NVMe: class=0x01 (Mass Storage), subclass=0x08 (NVM)
```

### Enabling a device

Before using a PCI device, enable memory space access and bus mastering:

```
function enable_device(ecam_base, bus, dev, func):
    cmd = read_u16(ecam_addr(..., 0x04))
    write_u16(ecam_addr(..., 0x04), cmd | 0x06)   // bits 1 and 2

function read_bar0_64(ecam_base, bus, dev, func) -> u64:
    bar0 = read_u32(ecam_addr(..., 0x10))
    bar1 = read_u32(ecam_addr(..., 0x14))
    return (bar0 & 0xFFFFFFF0) | (bar1 << 32)
```

---

## 14. NVMe Storage Driver

NVMe is a protocol for talking to SSDs over PCIe. It uses queue pairs in host memory: you write commands to a Submission Queue (SQ) and read results from a Completion Queue (CQ), ringing MMIO doorbells to notify the controller.

### Controller registers (BAR0, memory-mapped)

```
Offset  Size  Register
0x00    u64   CAP   (capabilities; bits [35:32] = doorbell stride)
0x14    u32   CC    (controller configuration; bit 0 = enable)
0x1C    u32   CSTS  (controller status; bit 0 = ready)
0x24    u32   AQA   (admin queue attributes)
0x28    u64   ASQ   (admin submission queue base address)
0x30    u64   ACQ   (admin completion queue base address)
0x1000+ u32   Doorbells (one pair per queue)
```

### DMA memory

The controller reads/writes directly to physical memory (DMA). Since we use identity mapping (virtual = physical), we can use static buffers. Allocate 6 page-aligned 4 KB pages:

```
Page 0: Admin SQ       (16 entries × 64 bytes)
Page 1: Admin CQ       (16 entries × 16 bytes)
Page 2: I/O SQ         (16 entries × 64 bytes)
Page 3: I/O CQ         (16 entries × 16 bytes)
Page 4: Identify buffer (4 KB, for admin data)
Page 5: Data buffer     (4 KB, for read/write sector data)
```

### Initialization

```
function nvme_init(ecam_base) -> NvmeController:
    (bus, dev, func) = find_device(ecam_base, class=0x01, subclass=0x08)
    bar = read_bar0_64(ecam_base, bus, dev, func)
    enable_device(ecam_base, bus, dev, func)

    cap = mmio_read64(bar + 0x00)
    stride = (cap >> 32) & 0xF

    // 1. Disable controller
    cc = mmio_read32(bar + 0x14)
    if cc & 1:
        mmio_write32(bar + 0x14, cc & ~1)
        while mmio_read32(bar + 0x1C) & 1: spin   // wait for not ready

    // 2. Zero the queue memory
    memset(admin_sq_page, 0, 4096)
    memset(admin_cq_page, 0, 4096)

    // 3. Configure admin queues
    aqa = (15 << 16) | 15                          // 16 entries each (0-based)
    mmio_write32(bar + 0x24, aqa)
    mmio_write64(bar + 0x28, admin_sq_address)
    mmio_write64(bar + 0x30, admin_cq_address)

    // 4. Enable controller
    cc = 1 | (6 << 16) | (4 << 20)                 // EN, IOSQES=64B, IOCQES=16B
    mmio_write32(bar + 0x14, cc)
    while (mmio_read32(bar + 0x1C) & 1) == 0: spin // wait for ready

    // 5. Issue admin commands
    identify_controller()     // opcode 0x06, CNS=1
    create_io_cq()            // opcode 0x05, QID=1
    create_io_sq()            // opcode 0x01, QID=1, linked to CQ 1
    identify_namespace()      // opcode 0x06, CNS=0, NSID=1 → get sector size
```

### Submission and completion

```
function submit(queue, command):
    volatile_write(sq[tail], command)            // write entry
    tail = (tail + 1) % QUEUE_DEPTH
    memory_fence()
    mmio_write32(bar + sq_doorbell_offset, tail) // ring doorbell

function wait_completion(queue) -> status:
    loop:
        entry = volatile_read(cq[head])
        if (entry.status & 1) == expected_phase:     // phase bit matches
            status = entry.status >> 1
            head = (head + 1) % QUEUE_DEPTH
            if head == 0: expected_phase = !expected_phase  // flip phase
            mmio_write32(bar + cq_doorbell_offset, head)
            return status
        spin
```

The phase bit is how NVMe signals new completions: the controller toggles it each time the queue wraps. You track the expected phase and compare.

### Doorbell offsets

```
sq_doorbell(qid) = 0x1000 + (2 * qid)     * (4 << stride)
cq_doorbell(qid) = 0x1000 + (2 * qid + 1) * (4 << stride)
```

### Read and write sectors

```
function nvme_read_sector(lba, buffer):
    cmd = new SqEntry()
    cmd.opcode = 0x02             // read
    cmd.nsid = 1
    cmd.prp1 = data_buffer_addr   // DMA target
    cmd.cdw10 = lba & 0xFFFFFFFF
    cmd.cdw11 = lba >> 32
    cmd.cdw12 = 0                 // 1 sector (0-based count)
    submit(io_sq, cmd)
    wait_completion(io_cq)
    memcopy(data_buffer, buffer, sector_size)

function nvme_write_sector(lba, buffer):
    memcopy(buffer, data_buffer, sector_size)
    cmd = new SqEntry()
    cmd.opcode = 0x01             // write
    // ... same fields as read
    submit(io_sq, cmd)
    wait_completion(io_cq)
```

The NVMe controller implements the `BlockDevice` interface (read_sector / write_sector), so the filesystem layer can use it directly.

---

## 15. A Filesystem

Design a minimal filesystem. No directories, no permissions, no journaling. Just files.

### Getting files to the kernel: the initial ramdisk

To load a font or other data files at boot, the bootloader can load a filesystem image into memory alongside the kernel. This is the *initial ramdisk* (initrd).

**Grow KernelArgs one last time:**

```
struct KernelArgs:
    memory_map_addr         u64
    memory_map_size         u64
    kernel_memory_addr      u64
    kernel_memory_size      u64
    framebuffer_addr        u64
    framebuffer_size        u64
    framebuffer_width       u32
    framebuffer_height      u32
    framebuffer_stride      u32
    framebuffer_pixel_format u32
    rsdp_addr               u64
    initrd_addr             u64     // ← new
    initrd_size             u64     // ← new
```

The build script packs files into the initrd image. The bootloader loads it from the FAT32 partition and passes the address to the kernel. The kernel mounts it as a read-only ramdisk.

### Block device abstraction

The filesystem shouldn't care whether it's talking to a ramdisk or an NVMe drive. Define an interface:

```
interface BlockDevice:
    sector_size() -> u32
    read_sector(lba, buffer)
    write_sector(lba, buffer)
```

Implement this for:
- **Memory slice**: wraps a raw pointer + length (for the initrd ramdisk)
- **NVMe controller**: wraps the driver from chapter 14

Add a **caching wrapper** that buffers one sector and handles byte-level reads/writes across sector boundaries:

```
struct Disk:
    device:      BlockDevice
    cache_buf:   byte array (sector_size bytes)
    cache_lba:   optional u64
    cache_dirty: bool

function disk_read(offset, buffer):
    while buffer not fully filled:
        lba = offset / sector_size
        if cache_lba != lba:
            flush()
            device.read_sector(lba, cache_buf)
            cache_lba = lba
        sector_offset = offset % sector_size
        copy min(remaining, sector_size - sector_offset) bytes
        advance offset and buffer position

function disk_write(offset, data):
    // same sector-splitting logic, but write to cache_buf and mark dirty

function flush():
    if cache_dirty:
        device.write_sector(cache_lba, cache_buf)
        cache_dirty = false
```

### On-disk layout

```
Offset 0:         Header (64 bytes)
                    [0..4]    magic ("TYFS" or whatever you choose)
                    [4..8]    version (u32)
                    [8..16]   disk_size (u64)
                    [16..24]  data_end (u64, next free byte for file data)
                    [24..32]  toc_start (u64, first ToC entry offset)

Offset 64:        File data (grows upward →)
                    file1_data | file2_data | file3_data | ...

Offset data_end:  [free space]

Offset toc_start: Table of Contents (grows ← downward)
                    entry_N | ... | entry_2 | entry_1

Offset disk_size: End
```

Data grows up. The ToC grows down. They meet in the middle when the disk is full.

### ToC entry (64 bytes)

```
[0]       flags    u8   (0 = free, 1 = in-use)
[1..32]   name     31 bytes, null-terminated
[32..40]  offset   u64  (byte offset of file data)
[40..48]  size     u64  (file size in bytes)
[48..64]  reserved
```

### Operations

```
function create(name, data) -> bool:
    if data.len + 64 > toc_start - data_end: return false  // full

    disk.write(data_end, data)                     // write file data
    entry = new ToC entry { flags=1, name, offset=data_end, size=data.len }
    toc_start -= 64
    disk.write(toc_start, entry)                   // write ToC entry
    data_end += data.len
    update_header()
    return true

function read_file(name) -> optional bytes:
    entry = find_entry(name)
    if entry is None: return None
    buffer = allocate(entry.size)
    disk.read(entry.offset, buffer)
    return buffer

function delete(name) -> bool:
    entry_offset = find_entry_offset(name)
    if entry_offset is None: return false
    disk.write(entry_offset, [0])    // clear flags byte → marks as free
    disk.flush()
    return true

function list() -> array of (name, size):
    results = []
    offset = toc_start
    while offset + 64 <= disk_size:
        entry = disk.read(offset, 64 bytes)
        if entry.flags == 1:
            results.append((entry.name, entry.size))
        offset += 64
    return results
```

### Mounting vs formatting

```
function mount(disk) -> optional Filesystem:
    header = disk.read(0, 64)
    if header[0..4] != "TYFS": return None     // not formatted
    return Filesystem { disk, data_end, toc_start, ... }

function format(disk, disk_size) -> Filesystem:
    write header with data_end=64, toc_start=disk_size
    return Filesystem { ... }
```

### NVMe persistence

On boot, peek at sector 0 of the NVMe drive. If the magic bytes match, mount it. Otherwise, format it. Files written to this filesystem survive reboots:

```
// In kernel init:
nvme_controller = nvme_init(ecam_base)
magic = nvme_controller.read_sector(0)[0..4]
disk = new CachingDisk(nvme_controller)
if magic == "TYFS":
    nvme_fs = mount(disk)
else:
    nvme_fs = format(disk, nvme_controller.total_bytes())
```

---

## 16. An Interactive Shell

### Command loop

```
function shell(initrd_fs, nvme_fs):
    cwd = "/nvme"
    line_buffer = [0; 256]
    line_len = 0

    print_prompt(cwd)

    loop:
        ch = try_read_char()
        if ch is None: spin; continue

        if ch == '\n':
            input = line_buffer[0..line_len] as string, trimmed
            execute(input, cwd, initrd_fs, nvme_fs)
            line_len = 0
            print_prompt(cwd)

        else if ch == BACKSPACE:
            if line_len > 0:
                line_len -= 1
                console_backspace()

        else:
            if line_len < 256:
                line_buffer[line_len] = ch
                line_len += 1
                console_putchar(ch)
```

### VFS: mount points as directories

The simplest "directory" abstraction — no changes to the filesystem needed:

```
/               virtual root (lists mount points)
/initrd         ramdisk filesystem (system files)
/nvme           NVMe persistent filesystem (user files)
```

```
function resolve_path(cwd, arg) -> (mount_name, filename):
    if arg starts with '/':
        full = arg
    else if cwd == "/":
        full = "/" + arg
    else:
        full = cwd + "/" + arg

    strip trailing slashes
    split on first '/' after root → (mount, file)
    return (mount, file)
```

### Commands

```
function execute(input, cwd, initrd_fs, nvme_fs):
    (cmd, arg) = split input on first space

    match cmd:
        "help"     → print command list
        "clear"    → clear screen
        "shutdown" → acpi_shutdown()
        "pwd"      → print cwd
        "cd"       → change cwd (validate target is "/" or "/initrd" or "/nvme")
        "ls"       → resolve path, list files from the correct filesystem
        "cat"      → resolve path, read file, print contents
        "rm"       → resolve path, delete file
        "write"    → resolve path, enter text input mode
        "edit"     → resolve path, show current contents, enter text input mode
```

### Text input mode (for write/edit)

When the user types `write myfile.txt`, switch to a line-accumulation mode:

```
editing_file = (mount, filename)
text_buffer = []

// In the command loop, while editing_file is set:
if line == ".":
    fs.delete(filename)
    fs.create(filename, text_buffer)
    print "File saved."
    editing_file = None
else:
    text_buffer.append(line + "\n")

// Change prompt from ">" to "|" to indicate input mode
```

### Prompt

Show the current directory:

```
function print_prompt(cwd):
    if editing_file:
        print "| "
    else:
        print cwd + "> "
```

---

## What You Have

At this point, your OS can:

- Boot from UEFI on any x86_64 machine (or QEMU)
- Display text on the framebuffer and serial console
- Accept keyboard input
- Allocate and free memory dynamically
- Enumerate PCI devices
- Read and write sectors on an NVMe SSD
- Store files in a custom filesystem that persists across reboots
- Navigate between mount points with `cd`, `ls`, `pwd`
- Create, edit, print, and delete files from an interactive shell
- Shut down cleanly via ACPI

All of this in roughly 2000 lines of kernel code.
