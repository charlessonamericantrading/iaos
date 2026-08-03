//! Real CMOS Real-Time Clock (RTC) access via the standard legacy I/O
//! ports (0x70 index / 0x71 data) - the same clock chip a real PC's BIOS
//! has always exposed this way, still faithfully emulated by QEMU's
//! default machine and synced to the host's wall-clock time by default.

use x86_64::instructions::port::Port;

const CMOS_ADDRESS: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

const STATUS_A_UPDATE_IN_PROGRESS: u8 = 0x80;
const STATUS_B_BINARY_MODE: u8 = 0x04;
const STATUS_B_24_HOUR: u8 = 0x02;
const HOURS_PM_FLAG: u8 = 0x80;

/// Wall-clock time as read from the CMOS RTC, already normalized to plain
/// binary values (converted out of BCD if that's the mode in use) and
/// 24-hour time - never raw hardware-format bytes. `year` assumes 20xx:
/// the RTC's year register is only two digits, and there's no universally
/// reliable century register to read instead.
pub struct RtcTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
}

fn read_register(reg: u8) -> u8 {
    unsafe {
        let mut addr_port: Port<u8> = Port::new(CMOS_ADDRESS);
        addr_port.write(reg);
        let mut data_port: Port<u8> = Port::new(CMOS_DATA);
        data_port.read()
    }
}

/// One CMOS read of all 7 registers `read_time` needs, after waiting out
/// any Update-In-Progress window. Waiting for UIP to *clear* before the
/// first register read only proves an update wasn't already underway -
/// it doesn't stop the *next* one-per-second update from beginning again
/// while these 7 sequential port reads are still in flight, which would
/// silently produce a torn value (some fields already advanced to the
/// new second, others not). `read_time` calls this twice in a row and
/// compares, retrying until two consecutive reads agree, the standard
/// CMOS RTC read protocol - fixing the 10th Explore-agent survey's
/// candidate #3, a real but rare/hard-to-force-in-CI gap distinct from
/// the midnight-conversion bug Fase 144 already fixed in this same file.
fn read_raw_registers() -> (u8, u8, u8, u8, u8, u8, u8) {
    while read_register(REG_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0 {
        core::hint::spin_loop();
    }
    (
        read_register(REG_SECONDS),
        read_register(REG_MINUTES),
        read_register(REG_HOURS),
        read_register(REG_DAY),
        read_register(REG_MONTH),
        read_register(REG_YEAR),
        read_register(REG_STATUS_B),
    )
}

fn bcd_to_binary(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
}

/// Converts an already-decoded 1-12 hour value (BCD/binary resolved, PM
/// bit masked off) into 24-hour form, using the PM flag read from the
/// *original* raw hours byte. Split out from `read_time` as its own
/// function so the AM/PM boundary cases - real hardware only ever
/// presents them at midnight/noon, impossible to force via QEMU's own
/// wall-clock in CI - can be exercised directly with synthetic inputs.
pub fn normalize_hours_12h(hours_raw: u8, hours_12: u8) -> u8 {
    if hours_raw & HOURS_PM_FLAG != 0 {
        // PM: 12 PM -> 12, 1-11 PM -> 13-23.
        (hours_12 % 12) + 12
    } else if hours_12 == 12 {
        // 12 AM (midnight) -> 0. Every other AM hour (1-11) is already
        // correct as-is in 24-hour form.
        0
    } else {
        hours_12
    }
}

/// Reads the current wall-clock time from the CMOS RTC.
///
/// Gets the 7 raw registers from `read_raw_registers` (which itself waits
/// out any in-progress hardware update, then reads all 7) and retries
/// until two consecutive reads agree, guarding against the update that
/// can begin again mid-read - see that function's own doc comment for
/// the full reasoning.
///
/// Also checks Status Register B rather than assuming BCD/24-hour mode:
/// real hardware and QEMU's emulation can both be configured either way,
/// and getting this wrong silently would misread every value (e.g.
/// treating BCD `0x59` as decimal 89).
pub fn read_time() -> RtcTime {
    let mut raw = read_raw_registers();
    loop {
        let raw_again = read_raw_registers();
        if raw == raw_again {
            break;
        }
        raw = raw_again;
    }
    let (seconds_raw, minutes_raw, hours_raw, day_raw, month_raw, year_raw, status_b) = raw;

    let is_binary = status_b & STATUS_B_BINARY_MODE != 0;
    let is_24h = status_b & STATUS_B_24_HOUR != 0;

    let (seconds, minutes, mut hours, day, month, year) = if is_binary {
        (
            seconds_raw,
            minutes_raw,
            hours_raw & !HOURS_PM_FLAG,
            day_raw,
            month_raw,
            year_raw,
        )
    } else {
        (
            bcd_to_binary(seconds_raw),
            bcd_to_binary(minutes_raw),
            bcd_to_binary(hours_raw & !HOURS_PM_FLAG),
            bcd_to_binary(day_raw),
            bcd_to_binary(month_raw),
            bcd_to_binary(year_raw),
        )
    };

    if !is_24h {
        hours = normalize_hours_12h(hours_raw, hours);
    }

    RtcTime {
        year: 2000 + year as u16,
        month,
        day,
        hours,
        minutes,
        seconds,
    }
}
