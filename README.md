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
| Agent scheduler (`ps`-visible) | Partial, and **not yet unified** with the context-switch primitive below - this is still just a priority-ordered process *table* (real bookkeeping, what `ps` reads), with no actual stack/registers behind each entry yet. |
| Context switch / cooperative task scheduler | Real, **hand-written asm - still flagged for human review** (not yet reviewed as of this note; built further on it anyway per explicit instruction, not by defaulting past the flag). `scheduler/context_switch.rs`'s `#[unsafe(naked)]` `switch_to` swaps RSP + the 6 callee-saved registers between stacks (xv6-style). Now generalized from one hardcoded worker to `spawn_cooperative`/`yield_now`/`finish_current_task` - a real priority-ordered N-task cooperative scheduler (up to 4 tasks) on the same primitive. Verified against a hand-traced expected output *before* running it: 3 tasks (High/Normal/Background) each print twice: the exact predicted serial order (alpha both iterations, then bravo both, then charlie both, then back to kernel) matched exactly, including the no-op self-switch path that guards against a stale-`saved_rsp` corruption hazard when the highest-priority task keeps re-winning against itself. Still cooperative, not preemptive - nothing here is wired to the timer IRQ yet, and this task scheduler is separate from the PCB table above until those get unified. |
| Interactive shell | Real (small). The `AgentOS>` prompt is IRQ1-driven (not polled), buffers a real line (heap-backed `String`) with backspace support, and dispatches `help` / `ps` / `mem` / `clear` against the live scheduler, KV-cache, and heap state - not canned output. No line history yet. |
| KV-cache memory manager | Real (small). Each block is a genuine heap allocation (`Box<[u8]>`, 4 KiB - sized for the current 100 KiB demo heap, not a realistic model-serving size yet); freeing a block really deallocates it. Previously used a fabricated address (`0x2000000 + idx*size`) that nothing was ever mapped to. |
| GGUF model loader / tensor engine | Simulated. Parses a hardcoded 24-byte sample header and runs a toy 4x4 matmul+ReLU; does not load or run a real model file. |
| VirtIO-net / TCP/IP stack | Simulated. Prints a fixed MAC address and increments packet counters; does not touch real VirtIO MMIO/virtqueues. |
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
