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
/// Left/Right Shift make codes. Unlike every other key handled here,
/// Shift is a held *state*, not a one-shot press - both these and their
/// break codes below matter.
const SCANCODE_LSHIFT: u8 = 0x2A;
const SCANCODE_RSHIFT: u8 = 0x36;
/// Break (key-up) codes for the two Shift keys - just the make code with
/// the top bit set, same PS/2 set 1 convention as everything else.
const SCANCODE_LSHIFT_BREAK: u8 = 0xAA;
const SCANCODE_RSHIFT_BREAK: u8 = 0xB6;

pub struct KeyboardDriver {
    last_scancode: u8,
    /// Set after seeing `SCANCODE_EXTENDED_PREFIX`, consumed by the very
    /// next scancode - that's the one that actually identifies the key.
    pending_extended: bool,
    /// True while either Shift key is held down. Doesn't distinguish
    /// left/right - nothing here needs to.
    shift_held: bool,
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
            shift_held: false,
            line_buffer: String::new(),
            cursor_pos: 0,
            history: Vec::new(),
            history_index: None,
        }
    }

    /// Convert PS/2 set 1 scancode to ASCII char, honoring the current
    /// Shift state for letters and the punctuation keys real shell usage
    /// actually needs: `~`/`-` (for "KERNEL~1"-style filenames), and -
    /// Fase 141 - `.`/`/` (`cat`/`touch` require `.` in every FILENAME.EXT
    /// argument, `parse_ipv4` requires `.` in every dotted IPv4 argument
    /// to `ping`/`dns`/`tcpecho`, and `split_path` requires `/` for every
    /// multi-segment path argument to `ls`/`mkdir`/`rmdir`/`touch`/`cat`/
    /// `rm` - a real, previously invisible-to-CI gap, since every existing
    /// self-test using a `.`/`/`-containing name dispatches it directly
    /// via `shell::dispatch_command`, bypassing this driver entirely).
    /// Fase 155 adds `,` (0x33 sits right between `m` (0x32) and `.`
    /// (0x34) in the standard PS/2 Set 1 layout, but had no arm at all -
    /// `touch`'s own free-form file content is the concrete, always-
    /// reachable case: any comma a real person typed there vanished
    /// silently, with no error). Deliberately NOT covering shifted
    /// digit-row symbols (`!@#$` etc.) or other punctuation - nothing on
    /// this disk or in any shell command needs them yet, and this is a
    /// shell keyboard driver, not a general-purpose one.
    pub fn scancode_to_char(&self, scancode: u8) -> Option<char> {
        let shift = self.shift_held;
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
            0x0C => Some(if shift { '_' } else { '-' }),
            0x10 => Some(if shift { 'Q' } else { 'q' }),
            0x11 => Some(if shift { 'W' } else { 'w' }),
            0x12 => Some(if shift { 'E' } else { 'e' }),
            0x13 => Some(if shift { 'R' } else { 'r' }),
            0x14 => Some(if shift { 'T' } else { 't' }),
            0x15 => Some(if shift { 'Y' } else { 'y' }),
            0x16 => Some(if shift { 'U' } else { 'u' }),
            0x17 => Some(if shift { 'I' } else { 'i' }),
            0x18 => Some(if shift { 'O' } else { 'o' }),
            0x19 => Some(if shift { 'P' } else { 'p' }),
            0x1E => Some(if shift { 'A' } else { 'a' }),
            0x1F => Some(if shift { 'S' } else { 's' }),
            0x20 => Some(if shift { 'D' } else { 'd' }),
            0x21 => Some(if shift { 'F' } else { 'f' }),
            0x22 => Some(if shift { 'G' } else { 'g' }),
            0x23 => Some(if shift { 'H' } else { 'h' }),
            0x24 => Some(if shift { 'J' } else { 'j' }),
            0x25 => Some(if shift { 'K' } else { 'k' }),
            0x26 => Some(if shift { 'L' } else { 'l' }),
            0x29 => Some(if shift { '~' } else { '`' }),
            0x2C => Some(if shift { 'Z' } else { 'z' }),
            0x2D => Some(if shift { 'X' } else { 'x' }),
            0x2E => Some(if shift { 'C' } else { 'c' }),
            0x2F => Some(if shift { 'V' } else { 'v' }),
            0x30 => Some(if shift { 'B' } else { 'b' }),
            0x31 => Some(if shift { 'N' } else { 'n' }),
            0x32 => Some(if shift { 'M' } else { 'm' }),
            0x33 => Some(if shift { '<' } else { ',' }),
            0x34 => Some(if shift { '>' } else { '.' }),
            0x35 => Some(if shift { '?' } else { '/' }),
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

    // Shift's release (break code) has to update state too, unlike every
    // other key here where only the press does anything - so this is
    // handled before the repeat-suppression logic below, which would
    // otherwise just discard it (it only ever recognizes make codes as
    // meaningful, tracking break codes solely so the *next* press of the
    // same key isn't mistaken for a held-key repeat).
    match scancode {
        SCANCODE_LSHIFT | SCANCODE_RSHIFT => {
            kb.shift_held = true;
            kb.last_scancode = scancode;
            return;
        }
        SCANCODE_LSHIFT_BREAK | SCANCODE_RSHIFT_BREAK => {
            kb.shift_held = false;
            kb.last_scancode = scancode;
            return;
        }
        _ => {}
    }

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
