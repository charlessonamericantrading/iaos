//! Minimal ATA PIO (Programmable I/O) disk read - the classic, simple,
//! universally-supported way to talk to an IDE/ATA disk, predating AHCI.
//! Read-only for now (only READ SECTORS, command `0x20`) - groundwork for
//! eventually loading a real file (a GGUF model, say) off disk instead of
//! a hardcoded sample, not a full disk driver yet.
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

const STATUS_ERR: u8 = 0x01;
const STATUS_DRQ: u8 = 0x08;
const STATUS_BSY: u8 = 0x80;

/// Generous but bounded - a real drive answers in microseconds, this is
/// purely a "never hang forever on hardware that isn't there or isn't
/// responding" backstop, not a realistic expected wait.
const MAX_POLL_ITERATIONS: u32 = 1_000_000;

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
        let mut drive_head: Port<u8> = Port::new(DRIVE_HEAD);
        let mut warmup: Port<u8> = Port::new(ERROR_FEATURES);
        let mut sector_count: Port<u8> = Port::new(SECTOR_COUNT);
        let mut lba_low: Port<u8> = Port::new(LBA_LOW);
        let mut lba_mid: Port<u8> = Port::new(LBA_MID);
        let mut lba_high: Port<u8> = Port::new(LBA_HIGH);
        let mut command_status: Port<u8> = Port::new(COMMAND_STATUS);
        let mut data: Port<u16> = Port::new(DATA);

        // 0xE0: LBA mode (bit 6) + master drive (bit 4 clear) + the two
        // reserved-as-1 bits (5, 7); low nibble is LBA bits 24-27.
        drive_head.write(0xE0 | ((lba >> 24) & 0x0F) as u8);

        // The drive needs ~400ns after a drive-select write before its
        // status register is meaningful - reading the (otherwise unused
        // here) error/features port a few times is the standard cheap way
        // to burn that time without a real timer.
        for _ in 0..4 {
            let _ = warmup.read();
        }

        sector_count.write(1);
        lba_low.write((lba & 0xFF) as u8);
        lba_mid.write(((lba >> 8) & 0xFF) as u8);
        lba_high.write(((lba >> 16) & 0xFF) as u8);
        command_status.write(CMD_READ_SECTORS);

        let mut status = poll_until(&mut command_status, |s| s & STATUS_BSY == 0)?;
        if status & STATUS_ERR != 0 {
            return Err("ATA read: drive reported an error (ERR bit set)");
        }

        status = poll_until(&mut command_status, |s| s & STATUS_DRQ != 0)?;
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

/// Polls `port` until `done(status)` is true, returning the status that
/// satisfied it - bounded by `MAX_POLL_ITERATIONS` so a non-responding
/// drive is a clean `Err`, not a hang.
unsafe fn poll_until(port: &mut Port<u8>, done: impl Fn(u8) -> bool) -> Result<u8, &'static str> {
    for _ in 0..MAX_POLL_ITERATIONS {
        let status = port.read();
        if done(status) {
            return Ok(status);
        }
    }
    Err("ATA read: timed out waiting for the drive to respond")
}
