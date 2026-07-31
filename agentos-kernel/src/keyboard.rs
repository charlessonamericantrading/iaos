use crate::kprint;
use alloc::string::String;
use alloc::vec::Vec;
use lazy_static::lazy_static;
use spin::Mutex;

/// PS/2 set 1 make-code for Backspace.
const SCANCODE_BACKSPACE: u8 = 0x0E;
/// PS/2 set 1: several keys (arrows among them) send this prefix byte
/// before their actual make/break code, e.g. Up-press is `0xE0, 0x48`.
const SCANCODE_EXTENDED_PREFIX: u8 = 0xE0;
const SCANCODE_UP: u8 = 0x48;
const SCANCODE_DOWN: u8 = 0x50;
const SCANCODE_LEFT: u8 = 0x4B;
const SCANCODE_RIGHT: u8 = 0x4D;

pub struct KeyboardDriver {
    last_scancode: u8,
    /// Set after seeing `SCANCODE_EXTENDED_PREFIX`, consumed by the very
    /// next scancode - that's the one that actually identifies the key.
    pending_extended: bool,
    line_buffer: String,
    /// Byte index into `line_buffer` where the next insert/backspace
    /// applies - always `<= line_buffer.len()`. Byte index and char index
    /// coincide here because `scancode_to_char` only ever produces
    /// single-byte ASCII, so every position is a valid char boundary;
    /// `String::insert`/`remove` would panic on a split multi-byte
    /// character otherwise.
    cursor_pos: usize,
    history: Vec<String>,
    /// `None` = a fresh line (not browsing history). `Some(i)` = currently
    /// showing `history[i]`, reachable by Up/Down.
    history_index: Option<usize>,
}

impl KeyboardDriver {
    pub const fn new() -> Self {
        KeyboardDriver {
            last_scancode: 0,
            pending_extended: false,
            line_buffer: String::new(),
            cursor_pos: 0,
            history: Vec::new(),
            history_index: None,
        }
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

    /// Moves to the previous (older) history entry, if any. Returns
    /// `true` if `line_buffer` changed (caller is responsible for
    /// updating the screen to match). Leaves the cursor at the end of the
    /// recalled line, same convention as a real shell.
    fn recall_older(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let new_index = match self.history_index {
            None => self.history.len() - 1,
            Some(0) => return false, // already at the oldest entry
            Some(i) => i - 1,
        };
        self.history_index = Some(new_index);
        self.line_buffer.clear();
        self.line_buffer.push_str(&self.history[new_index]);
        self.cursor_pos = self.line_buffer.len();
        true
    }

    /// Moves to the next (newer) history entry, or back to a fresh empty
    /// line once past the newest. Returns `true` if `line_buffer` changed.
    fn recall_newer(&mut self) -> bool {
        match self.history_index {
            None => false, // already a fresh line
            Some(i) if i + 1 < self.history.len() => {
                self.history_index = Some(i + 1);
                self.line_buffer.clear();
                self.line_buffer.push_str(&self.history[i + 1]);
                self.cursor_pos = self.line_buffer.len();
                true
            }
            Some(_) => {
                self.history_index = None;
                self.line_buffer.clear();
                self.cursor_pos = 0;
                true
            }
        }
    }

    /// Inserts `ch` at the cursor (not necessarily at the end) and
    /// redraws the line to match.
    fn insert_char(&mut self, ch: char) {
        let old_len = self.line_buffer.len();
        let old_cursor = self.cursor_pos;
        self.line_buffer.insert(self.cursor_pos, ch);
        self.cursor_pos += 1;
        self.redraw_line(old_len, old_cursor);
    }

    /// Deletes the character just before the cursor (if any) and redraws.
    /// A no-op at the start of the line, same as a real shell.
    fn backspace_char(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let old_len = self.line_buffer.len();
        let old_cursor = self.cursor_pos;
        self.line_buffer.remove(self.cursor_pos - 1);
        self.cursor_pos -= 1;
        self.redraw_line(old_len, old_cursor);
    }

    /// The one fully general way to keep the screen in sync with
    /// `line_buffer`/`cursor_pos` after *any* edit - including one in the
    /// middle of the line, which shifts every character after it. Plain
    /// `backspace()` only ever erases from wherever the writer's cursor
    /// currently sits, moving further left - so to erase the *whole*
    /// previous line correctly regardless of where `old_cursor` was, this
    /// first moves right to the true old end (`cursor_right` for each
    /// character between `old_cursor` and `old_len`), then backspaces the
    /// full `old_len`, reprints the new `line_buffer` from scratch, and
    /// finally moves left back to the new `cursor_pos` (a no-op if the
    /// edit was at the end, which keeps the common case exactly as cheap
    /// as before this method existed).
    fn redraw_line(&self, old_len: usize, old_cursor: usize) {
        for _ in old_cursor..old_len {
            crate::vga_buffer::cursor_right();
        }
        for _ in 0..old_len {
            crate::vga_buffer::backspace();
        }
        kprint!("{}", self.line_buffer);
        for _ in self.cursor_pos..self.line_buffer.len() {
            crate::vga_buffer::cursor_left();
        }
    }
}

lazy_static! {
    pub static ref KEYBOARD: Mutex<KeyboardDriver> = Mutex::new(KeyboardDriver::new());
}

/// Called from the IRQ1 handler in `interrupts.rs` with the scancode
/// already read off PS/2 port 0x60. PS/2 set 1 sends one code on key-down
/// and the same code with the top bit set on key-up, and can repeat the
/// down-code while a key is held - we only act on a genuinely new key-down
/// (this applies equally to the byte following an extended `0xE0` prefix).
///
/// Accumulates printable characters into a per-driver line buffer and
/// hands the completed line to `shell::dispatch_command` on Enter.
/// Backspace/typing act at the cursor, not always at the end of the line;
/// Left/Right move the cursor without changing the buffer. Up/Down browse
/// real command history, replacing the whole line and leaving the cursor
/// at its end.
pub fn handle_scancode(scancode: u8) {
    let mut kb = KEYBOARD.lock();

    if scancode == SCANCODE_EXTENDED_PREFIX {
        kb.pending_extended = true;
        return;
    }
    let is_extended = core::mem::take(&mut kb.pending_extended);

    let is_new_press = scancode != kb.last_scancode && (scancode & 0x80) == 0;
    kb.last_scancode = scancode;
    if !is_new_press {
        return;
    }

    if is_extended {
        match scancode {
            SCANCODE_UP | SCANCODE_DOWN => {
                let old_len = kb.line_buffer.len();
                let old_cursor = kb.cursor_pos;
                let changed = if scancode == SCANCODE_UP {
                    kb.recall_older()
                } else {
                    kb.recall_newer()
                };
                if changed {
                    kb.redraw_line(old_len, old_cursor);
                }
            }
            SCANCODE_LEFT if kb.cursor_pos > 0 => {
                kb.cursor_pos -= 1;
                crate::vga_buffer::cursor_left();
            }
            SCANCODE_RIGHT if kb.cursor_pos < kb.line_buffer.len() => {
                kb.cursor_pos += 1;
                crate::vga_buffer::cursor_right();
            }
            _ => {}
        }
        return;
    }

    if scancode == SCANCODE_BACKSPACE {
        kb.history_index = None; // editing means we're on a fresh line again
        kb.backspace_char();
        return;
    }

    if let Some(ch) = kb.scancode_to_char(scancode) {
        if ch == '\n' {
            let line = core::mem::take(&mut kb.line_buffer);
            kb.cursor_pos = 0;
            kb.history_index = None;
            if !line.trim().is_empty() {
                kb.history.push(line.clone());
            }
            drop(kb);
            kprint!("\n");
            crate::shell::dispatch_command(&line);
            kprint!("AgentOS> ");
        } else {
            kb.history_index = None;
            kb.insert_char(ch);
        }
    }
}
