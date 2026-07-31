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

fn bcd_to_binary(value: u8) -> u8 {
    (value & 0x0F) + ((value >> 4) * 10)
}

/// Reads the current wall-clock time from the CMOS RTC.
///
/// Waits out any in-progress hardware update first (Status Register A's
/// Update-In-Progress bit) - the chip updates its registers roughly once
/// a second, and reading mid-update can return a torn value (some fields
/// already advanced to the new second, others not yet), a real if
/// uncommon source of an occasional nonsensical read if ignored.
///
/// Also checks Status Register B rather than assuming BCD/24-hour mode:
/// real hardware and QEMU's emulation can both be configured either way,
/// and getting this wrong silently would misread every value (e.g.
/// treating BCD `0x59` as decimal 89).
pub fn read_time() -> RtcTime {
    while read_register(REG_STATUS_A) & STATUS_A_UPDATE_IN_PROGRESS != 0 {
        core::hint::spin_loop();
    }

    let seconds_raw = read_register(REG_SECONDS);
    let minutes_raw = read_register(REG_MINUTES);
    let hours_raw = read_register(REG_HOURS);
    let day_raw = read_register(REG_DAY);
    let month_raw = read_register(REG_MONTH);
    let year_raw = read_register(REG_YEAR);
    let status_b = read_register(REG_STATUS_B);

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

    if !is_24h && hours_raw & HOURS_PM_FLAG != 0 {
        // 12-hour mode, PM flag set - checked on the *raw* hours byte,
        // since the branch above already masked that bit off before any
        // BCD/binary conversion.
        hours = (hours % 12) + 12;
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
