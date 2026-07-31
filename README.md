# AgentOS

[![kernel-ci](https://github.com/charlessonamericantrading/iaos/actions/workflows/kernel-ci.yml/badge.svg)](https://github.com/charlessonamericantrading/iaos/actions/workflows/kernel-ci.yml)

An "AI-native" operating system project, explored in two independent, **not yet integrated** directions:

1. **[`agentos-kernel/`](agentos-kernel/)** — a real bare-metal kernel written in Rust (`#![no_std]`), booting on actual x86_64 hardware/QEMU. This is the actively developed half of the project.
2. **[`src/`](src/) + [`public/`](public/)** — a TypeScript/Express dashboard that simulates what an AI-agent OS's control plane might look like (scheduler, memory, sandbox). It is a UI mock: no real model calls, no persistence, everything in-memory. Parked in favor of the kernel.

There is no bridge between the two yet — `src/bridge.ts` only POSTs one canned sample event to demonstrate the shape of an API, it does not talk to the kernel.

## Current status (honest version)

The kernel **boots for real** in QEMU via the [`bootloader`](https://github.com/rust-osdev/bootloader) crate and reaches an interactive prompt. What's actually real hardware/software vs. simulated scaffolding, as of this commit:

| Subsystem | Status |
|---|---|
| Boot (`bootloader` crate, BIOS) | **Real.** Verified boot to `AgentOS>` prompt. |
| GDT / IDT / CPU exceptions | **Real.** breakpoint, double-fault (own IST stack), page-fault, GPF, divide-error, invalid-opcode all wired and verified via a live `int3` self-test. |
| 8259 PIC + hardware interrupts | **Real.** PIC remapped to vectors 32-47 (avoids colliding with CPU exception vectors 8-15), interrupts enabled, timer (IRQ0) and keyboard (IRQ1) are interrupt-driven, not polled. |
| Physical memory / paging / heap | **Real.** Frame allocator walks the actual `BootInfo` memory map (skips regions already used by the kernel/bootloader); the heap is properly mapped page-by-page before use. `alloc` (`Vec`, `Box`, `String`) works. |
| Agent scheduler (`ps`-visible) | Partial, and **not yet unified** with the schedulers below - this is still just a priority-ordered process *table* (real bookkeeping, what `ps` reads), with no actual stack/registers behind each entry yet. |
| Cooperative task scheduler | Real. `scheduler/context_switch.rs`'s `#[unsafe(naked)]` `switch_to` swaps RSP + the 6 callee-saved registers between stacks (xv6-style) - `spawn_cooperative`/`yield_now`/`finish_current_task` build a priority-ordered N-task cooperative scheduler (up to 4 tasks) on top of it. Verified against a hand-traced expected output decided *before* running it, which matched exactly. A task only ever changes on its own voluntary `yield_now` call. |
| **Preemptive scheduler (real)** | Real. `scheduler/preemptive.rs` hooks the same `switch_to` primitive into the timer IRQ (`interrupts.rs`) so tasks that *never* call `yield_now` at all still get forced off the CPU - proven with two infinite, non-cooperating loops that both end up with large, nonzero counters after a timed window, which is only possible if the timer genuinely reclaimed the CPU from each without asking. Two real, subtle bugs surfaced and got fixed getting here, both the same root cause: state shared between an interrupt handler and normal code needs `AtomicBool`/`AtomicU64`, not a plain `static mut` - in a release+LTO build, the compiler hoisted a `while flag { hlt() }` poll loop's read out entirely (an infinite hang, since it never noticed the interrupt handler had cleared the flag), and separately treated a counter only ever incremented inside an infinite loop with no in-function reader as having no provable side effect. Neither was caught by clippy or by reasoning about the design alone - only by actually booting it and getting a genuinely wrong/hung result, then diagnosing why. |
| Context switch primitive (hand-written asm) | **Still flagged for human review** - not yet reviewed as of this note; built further on it anyway per the project owner's explicit instruction not to wait. Both schedulers above share it. |
| Interactive shell | Real (small). The `AgentOS>` prompt is IRQ1-driven (not polled), buffers a real line (heap-backed `String`) with backspace and Up/Down command history (a real `Vec<String>`), and dispatches `help` / `ps` / `mem` / `uptime` / `clear` against the live scheduler, KV-cache, heap, and timer-tick state - not canned output. No left/right cursor movement within a line yet. |
| KV-cache memory manager | Real (small). Each block is a genuine heap allocation (`Box<[u8]>`, 4 KiB - sized for the current 100 KiB demo heap, not a realistic model-serving size yet); freeing a block really deallocates it. Previously used a fabricated address (`0x2000000 + idx*size`) that nothing was ever mapped to. |
| GGUF model loader / tensor engine | Simulated. Parses a hardcoded 24-byte sample header and runs a toy 4x4 matmul+ReLU; does not load or run a real model file. |
| PCI bus enumeration | Real. `pci.rs` reads real config space via the legacy `CONFIG_ADDRESS`/`CONFIG_DATA` I/O ports (read-only - never reconfigures anything) and the `lspci` shell command lists what it finds. Verified against known-good hardware IDs, not just "didn't crash": QEMU's default machine reports exactly the expected i440FX host bridge (`8086:1237`), PIIX3/4 ISA/IDE/ACPI bridges (`8086:7000`/`7010`/`7113`), standard VGA (`1234:1111`), and an e1000 NIC (`8086:100e`) - all real, recognizable vendor:device IDs, not fabricated ones. Only scans bus 0 (no bridge recursion yet). |
| Disk I/O (ATA PIO) | Real (read-only). `ata.rs` reads real 512-byte sectors from the primary ATA master via classic PIO (ports 0x1F0-0x1F7) - the `disk` shell command reads LBA 0 and checks for the `0x55AA` MBR boot signature, which is genuinely present since it's reading our own disk image. **Caught a real bug getting here**: the drive raises IRQ14 on command completion regardless of whether anything is waiting on it, and this kernel had no handler for that vector at all - the first real read double-faulted the instant interrupts came back on after the operation. Fixed with a real (if minimal) `ata_primary_interrupt_handler` in `interrupts.rs`, not just by suppressing the symptom. |
| Partition table (MBR) | Real. `partition.rs` parses the 4 primary partition entries out of the boot sector already read via ATA. On our own disk image this finds two real bootable entries: a small internal region (the `bootloader` crate's own boot stages) and a partition typed `0x0C` ("FAT32 LBA") - **which turned out to actually be formatted FAT12**, see below. |
| FAT32 filesystem parsing | Real parser, correctly detects it doesn't apply here. `fat32.rs` parses the BPB and can walk cluster chains / list a directory - **but our own disk's `0x0C`-typed partition is actually FAT12**, confirmed by reading the raw boot sector (`root_entry_count=512`, and the on-disk filesystem-type string literally reads `"FAT12   "`, volume label `"agentos-ker"`) - the `mbrman`/`fatfs` tooling that builds our disk image apparently doesn't always match the MBR type byte to the filesystem it actually formats for a volume this small. **Found the hard way**: the first real attempt silently misread FAT12's differently-shaped BPB as FAT32 fields, computing a garbage LBA in the billions and returning a confusing raw ATA error. Fixed by checking `root_entry_count` (FAT32 always sets it to 0; FAT12/16 never do - the standard, authoritative discriminator) and failing with a clear, honest message instead. Real FAT12 support (12-bit packed FAT entries, fixed-location root directory - a different, fiddlier shape than FAT32) is deliberately not attempted yet - a distinct next step, not bundled into an already-long debugging session. |
| VirtIO-net / TCP/IP stack | Simulated. Prints a fixed MAC address and increments packet counters; does not touch real VirtIO MMIO/virtqueues. Now that PCI enumeration is real: note the *default* QEMU machine (no extra `-device` flags) exposes a real **e1000** NIC, not a virtio-net one - a genuinely-real version of this driver would need to either target e1000, or the boot command would need `-device virtio-net-pci` added first. |
| Syscall dispatcher | Minimal. A handful of syscall numbers wired to stub handlers. |

Nothing above is faked to look more finished than it is — this table is meant to stay accurate as the project grows, not to be a wishlist.

## Building & running the kernel

Requires the **nightly** Rust toolchain (pinned per-directory via `rust-toolchain.toml`) and [QEMU](https://www.qemu.org/).

```bash
# 1. Build the kernel itself
cd agentos-kernel
cargo build --release

# 2. Package it into a bootable disk image (separate crate, host target)
cd ../kernel-runner
cargo build --release
./target/release/kernel-runner "../agentos-kernel/target/x86_64-unknown-none/release/agentos-kernel" "../agentos-kernel/target/disk-image"

# 3. Boot it
qemu-system-x86_64 -drive format=raw,file=../agentos-kernel/target/disk-image/agentos-bios.img
```

Or just run [`agentos-kernel/boot_kernel.bat`](agentos-kernel/boot_kernel.bat), which automates all three steps and opens an interactive QEMU window.

For a headless run with serial output captured to your terminal instead:

```bash
qemu-system-x86_64 -drive format=raw,file=agentos-kernel/target/disk-image/agentos-bios.img -serial stdio -display none -no-reboot -no-shutdown
```

## Running the TypeScript dashboard simulator

```bash
npm install
npm run build
npm start
```

Serves the mocked dashboard UI in `public/` against the in-memory "kernel" in `src/server/kernel.ts`. Nothing here talks to real hardware or a real model.

## Repository layout

```
agentos-kernel/   Rust #![no_std] kernel - GDT/IDT, paging/heap, scheduler, syscalls, drivers
kernel-runner/    Host-target helper crate that packages the kernel ELF into a bootable disk image
src/, public/     TypeScript/Express dashboard simulator (parked, not integrated with the kernel)
```

## License

MIT - see [LICENSE](LICENSE).
