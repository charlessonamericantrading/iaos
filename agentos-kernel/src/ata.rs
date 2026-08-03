//! Minimal ATA PIO (Programmable I/O) disk read/write - the classic,
//! simple, universally-supported way to talk to an IDE/ATA disk,
//! predating AHCI. `read_sector`/`write_sector` (commands `0x20`/`0x30`) -
//! groundwork for real file I/O (a GGUF model, say, or real FAT write
//! support) instead of a hardcoded sample, not a full disk driver yet.
//!
//! Targets the primary ATA bus's master drive (legacy/compatibility-mode
//! ports 0x1F0-0x1F7) - the same ports the PIIX3 IDE controller `pci.rs`
//! found (device `8086:7010`) responds on without needing any further
//! PCI-level setup.

use x86_64::instructions::port::Port;

const DATA: u16 = 0x1F0;
const ERROR_FEATURES: u16 = 0x1F1;
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LOW: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HIGH: u16 = 0x1F5;
const DRIVE_HEAD: u16 = 0x1F6;
const COMMAND_STATUS: u16 = 0x1F7;

const CMD_READ_SECTORS: u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_CACHE_FLUSH: u8 = 0xE7;

const STATUS_ERR: u8 = 0x01;
const STATUS_DRQ: u8 = 0x08;
const STATUS_BSY: u8 = 0x80;

/// Generous but bounded - purely a "never hang forever on hardware that
/// isn't there or isn't responding" backstop, not a realistic expected
/// wait (a real or emulated drive answers within a handful of iterations
/// in the healthy path, so this bound is never actually walked in normal
/// operation). Sized for QEMU's software (TCG) port-I/O emulation, not
/// bare metal: each `in`/`out` on an emulated port costs a full VM-exit
/// and device-model dispatch, which under a loaded/shared CI runner can
/// cost far more wall-clock time per iteration than the same instruction
/// would on real hardware - so the same iteration count buys much less
/// real headroom there than the number alone suggests. Bumped from the
/// original 1,000,000 after a genuine (if rare) CI timeout hit exactly
/// this margin: restoring BOOT-S~2 (152136 bytes, ~297 sectors) in the
/// FAT12 write-test is the single most poll-intensive operation in the
/// whole boot sequence, and was the first to reveal the old bound wasn't
/// as generous under CI timing as its healthy-path behavior implied.
const MAX_POLL_ITERATIONS: u32 = 50_000_000;

/// Reads one 512-byte sector at 28-bit LBA `lba` from the primary ATA
/// bus's master drive into `buf`.
///
/// # Errors
/// Returns `Err` if the drive reports an error (`ERR` status bit) or
/// never becomes ready within a bounded number of polls.
///
/// Runs with interrupts disabled for its whole duration - standard
/// practice for a tight PIO polling sequence like this, so an unrelated
/// timer/keyboard IRQ can't add jitter in the middle of it. This does
/// *not* prevent the drive's own completion interrupt (IRQ14 on the
/// primary channel) - that fires and is handled the normal way (see
/// `interrupts::ata_primary_interrupt_handler`) once this returns and
/// interrupts come back on; it's just unrelated to how this function
/// gets its data, which is pure polling.
pub fn read_sector(lba: u32, buf: &mut [u8; 512]) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| read_sector_inner(lba, buf))
}

fn read_sector_inner(lba: u32, buf: &mut [u8; 512]) -> Result<(), &'static str> {
    unsafe {
        let mut command_status: Port<u8> = Port::new(COMMAND_STATUS);
        let mut data: Port<u16> = Port::new(DATA);

        let mut status = select_drive_and_issue_command(lba, "read", CMD_READ_SECTORS)?;
        if status & STATUS_ERR != 0 {
            return Err("ATA read: drive reported an error (ERR bit set)");
        }

        status = poll_until(&mut command_status, "read", |s| s & STATUS_DRQ != 0)?;
        if status & STATUS_ERR != 0 {
            return Err("ATA read: drive reported an error (ERR bit set)");
        }

        for chunk in buf.as_chunks_mut::<2>().0 {
            let word = data.read();
            chunk[0] = (word & 0xFF) as u8;
            chunk[1] = (word >> 8) as u8;
        }
    }
    Ok(())
}

/// Writes one 512-byte sector at 28-bit LBA `lba` on the primary ATA
/// bus's master drive from `buf`.
///
/// # Errors
/// Returns `Err` if the drive reports an error (`ERR` status bit) or
/// never becomes ready within a bounded number of polls - same
/// conventions as `read_sector`.
///
/// Issues CACHE FLUSH (`0xE7`) after the data transfer and waits for it
/// to complete before returning `Ok` - so a caller that gets `Ok` back
/// knows the write is genuinely committed, not just accepted into a
/// volatile write cache. QEMU's emulated IDE controller likely commits
/// synchronously already, but the flush costs one more bounded poll and
/// is what a real drive needs this guarantee from.
pub fn write_sector(lba: u32, buf: &[u8; 512]) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| write_sector_inner(lba, buf))
}

fn write_sector_inner(lba: u32, buf: &[u8; 512]) -> Result<(), &'static str> {
    unsafe {
        let mut command_status: Port<u8> = Port::new(COMMAND_STATUS);
        let mut data: Port<u16> = Port::new(DATA);

        let mut status = select_drive_and_issue_command(lba, "write", CMD_WRITE_SECTORS)?;
        if status & STATUS_ERR != 0 {
            return Err("ATA write: drive reported an error (ERR bit set)");
        }

        // DRQ here means "ready for you to hand over the data" - the
        // write-side mirror of read_sector's DRQ check meaning "data is
        // ready for you to take".
        status = poll_until(&mut command_status, "write", |s| s & STATUS_DRQ != 0)?;
        if status & STATUS_ERR != 0 {
            return Err("ATA write: drive reported an error (ERR bit set)");
        }

        for chunk in buf.as_chunks::<2>().0 {
            data.write((chunk[0] as u16) | ((chunk[1] as u16) << 8));
        }

        status = poll_until(&mut command_status, "write", |s| s & STATUS_BSY == 0)?;
        if status & STATUS_ERR != 0 {
            return Err("ATA write: drive reported an error after transfer (ERR bit set)");
        }

        command_status.write(CMD_CACHE_FLUSH);
        poll_until(&mut command_status, "write", |s| s & STATUS_BSY == 0)?;
    }
    Ok(())
}

/// Selects the master drive in LBA mode for `lba`, waits the standard
/// ~400ns drive-select settling time, writes the sector-count/LBA
/// registers, issues `command`, and waits for `BSY` to clear - the exact
/// setup `read_sector_inner`/`write_sector_inner` previously each hand-
/// duplicated verbatim (byte-for-byte identical except this final
/// `command` byte, the same "duplicate logic, unify it" class Fase 97/
/// 111/118/119/122/125/131/139 already found and fixed elsewhere in this
/// project). Returns the post-`BSY` status rather than checking `ERR`
/// itself, since both callers immediately did that same wait next anyway
/// but word their own `ERR` error message slightly differently
/// ("ATA read: ..." vs "ATA write: ...").
unsafe fn select_drive_and_issue_command(
    lba: u32,
    op: &'static str,
    command: u8,
) -> Result<u8, &'static str> {
    let mut drive_head: Port<u8> = Port::new(DRIVE_HEAD);
    let mut warmup: Port<u8> = Port::new(ERROR_FEATURES);
    let mut sector_count: Port<u8> = Port::new(SECTOR_COUNT);
    let mut lba_low: Port<u8> = Port::new(LBA_LOW);
    let mut lba_mid: Port<u8> = Port::new(LBA_MID);
    let mut lba_high: Port<u8> = Port::new(LBA_HIGH);
    let mut command_status: Port<u8> = Port::new(COMMAND_STATUS);

    // 0xE0: LBA mode (bit 6) + master drive (bit 4 clear) + the two
    // reserved-as-1 bits (5, 7); low nibble is LBA bits 24-27.
    drive_head.write(0xE0 | ((lba >> 24) & 0x0F) as u8);

    // The drive needs ~400ns after a drive-select write before its status
    // register is meaningful - reading the (otherwise unused here)
    // error/features port a few times is the standard cheap way to burn
    // that time without a real timer.
    for _ in 0..4 {
        let _ = warmup.read();
    }

    sector_count.write(1);
    lba_low.write((lba & 0xFF) as u8);
    lba_mid.write(((lba >> 8) & 0xFF) as u8);
    lba_high.write(((lba >> 16) & 0xFF) as u8);
    command_status.write(command);

    poll_until(&mut command_status, op, |s| s & STATUS_BSY == 0)
}

/// Polls `port` until `done(status)` is true, returning the status that
/// satisfied it - bounded by `MAX_POLL_ITERATIONS` so a non-responding
/// drive is a clean `Err`, not a hang. `op` labels the error message with
/// the real calling operation ("ATA read"/"ATA write") instead of a
/// hardcoded one - this helper is shared by both directions, and a
/// write-side timeout previously misreported itself as a read.
unsafe fn poll_until(
    port: &mut Port<u8>,
    op: &'static str,
    done: impl Fn(u8) -> bool,
) -> Result<u8, &'static str> {
    for _ in 0..MAX_POLL_ITERATIONS {
        let status = port.read();
        if done(status) {
            return Ok(status);
        }
    }
    match op {
        "read" => Err("ATA read: timed out waiting for the drive to respond"),
        _ => Err("ATA write: timed out waiting for the drive to respond"),
    }
}
