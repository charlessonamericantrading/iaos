use spin::Mutex;
use lazy_static::lazy_static;
use crate::{kprint, kprintln};

pub struct KeyboardDriver {
    last_scancode: u8,
}

impl KeyboardDriver {
    pub const fn new() -> Self {
        KeyboardDriver { last_scancode: 0 }
    }

    /// Convert PS/2 set 1 scancode to ASCII char
    pub fn scancode_to_char(&self, scancode: u8) -> Option<char> {
        match scancode {
            0x02 => Some('1'),
            0x03 => Some('2'),
            0x04 => Some('3'),
            0x05 => Some('4'),
            0x06 => Some('5'),
            0x07 => Some('6'),
            0x08 => Some('7'),
            0x09 => Some('8'),
            0x0A => Some('9'),
            0x0B => Some('0'),
            0x10 => Some('q'),
            0x11 => Some('w'),
            0x12 => Some('e'),
            0x13 => Some('r'),
            0x14 => Some('t'),
            0x15 => Some('y'),
            0x16 => Some('u'),
            0x17 => Some('i'),
            0x18 => Some('o'),
            0x19 => Some('p'),
            0x1E => Some('a'),
            0x1F => Some('s'),
            0x20 => Some('d'),
            0x21 => Some('f'),
            0x22 => Some('g'),
            0x23 => Some('h'),
            0x24 => Some('j'),
            0x25 => Some('k'),
            0x26 => Some('l'),
            0x2C => Some('z'),
            0x2D => Some('x'),
            0x2E => Some('c'),
            0x2F => Some('v'),
            0x30 => Some('b'),
            0x31 => Some('n'),
            0x32 => Some('m'),
            0x39 => Some(' '),
            0x1C => Some('\n'),
            _ => None,
        }
    }
}

lazy_static! {
    pub static ref KEYBOARD: Mutex<KeyboardDriver> = Mutex::new(KeyboardDriver::new());
}

/// Called from the IRQ1 handler in `interrupts.rs` with the scancode
/// already read off PS/2 port 0x60. PS/2 set 1 sends one code on key-down
/// and the same code with the top bit set on key-up, and can repeat the
/// down-code while a key is held - we only act on a genuinely new key-down.
pub fn handle_scancode(scancode: u8) {
    let mut kb = KEYBOARD.lock();
    let is_new_press = scancode != kb.last_scancode && (scancode & 0x80) == 0;
    kb.last_scancode = scancode;
    if !is_new_press {
        return;
    }

    if let Some(ch) = kb.scancode_to_char(scancode) {
        if ch == '\n' {
            kprintln!("\n[AGENTOS SHELL] Task dispatched to Kernel Scheduler.");
            kprint!("AgentOS> ");
        } else {
            kprint!("{}", ch);
        }
    }
}
