use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;
use spin::Mutex;
use volatile::Volatile;

/// Offset added to physical addresses to reach the bootloader's identity
/// mapping of physical memory (see `BootInfo::physical_memory_offset`).
/// Must be set from `kernel_main` before the first `kprint!`/`kprintln!`
/// call, since `WRITER` computes its buffer pointer on first use.
pub static PHYS_MEM_OFFSET: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ColorCode(u8);

impl ColorCode {
    fn new(foreground: Color, background: Color) -> ColorCode {
        ColorCode((background as u8) << 4 | (foreground as u8))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii_character: u8,
    color_code: ColorCode,
}

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

#[repr(transparent)]
struct Buffer {
    chars: [[Volatile<ScreenChar>; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

pub struct Writer {
    column_position: usize,
    color_code: ColorCode,
    buffer: &'static mut Buffer,
}

impl Writer {
    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            byte => {
                if self.column_position >= BUFFER_WIDTH {
                    self.new_line();
                }

                let row = BUFFER_HEIGHT - 1;
                let col = self.column_position;

                let color_code = self.color_code;
                self.buffer.chars[row][col].write(ScreenChar {
                    ascii_character: byte,
                    color_code,
                });
                self.column_position += 1;
            }
        }
    }

    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                0x20..=0x7e | b'\n' => self.write_byte(byte),
                _ => self.write_byte(0xfe),
            }
        }
    }

    fn new_line(&mut self) {
        for row in 1..BUFFER_HEIGHT {
            for col in 0..BUFFER_WIDTH {
                let character = self.buffer.chars[row][col].read();
                self.buffer.chars[row - 1][col].write(character);
            }
        }
        self.clear_row(BUFFER_HEIGHT - 1);
        self.column_position = 0;
    }

    fn clear_row(&mut self, row: usize) {
        let blank = ScreenChar {
            ascii_character: b' ',
            color_code: self.color_code,
        };
        for col in 0..BUFFER_WIDTH {
            self.buffer.chars[row][col].write(blank);
        }
    }

    pub fn clear_screen(&mut self) {
        for row in 0..BUFFER_HEIGHT {
            self.clear_row(row);
        }
        self.column_position = 0;
    }

    /// Erases the last character on the current line. Only handles the
    /// single-line case (does nothing at column 0) - the shell prompt is
    /// always a fresh line after Enter, so backspacing across a wrapped
    /// line boundary isn't a real scenario here yet.
    pub fn backspace(&mut self) {
        if self.column_position > 0 {
            self.column_position -= 1;
            let row = BUFFER_HEIGHT - 1;
            let col = self.column_position;
            self.buffer.chars[row][col].write(ScreenChar {
                ascii_character: b' ',
                color_code: self.color_code,
            });
        }
    }

    /// Moves the write cursor one column left *without* erasing what's
    /// there - unlike `backspace`, the character stays on screen. Used for
    /// pure cursor movement (Left arrow) and internally by callers that
    /// need to reposition after reprinting a line, where nothing about the
    /// line's on-screen content should change, only where the next
    /// write/erase lands.
    pub fn cursor_left(&mut self) {
        if self.column_position > 0 {
            self.column_position -= 1;
        }
    }

    /// Moves the write cursor one column right without writing anything.
    /// The `BUFFER_WIDTH` clamp is only a hard physical backstop - callers
    /// are expected to already know (from their own line-length
    /// bookkeeping) exactly how far it's valid to move, same as
    /// `cursor_left` relies on callers not calling it more times than
    /// there are characters to move back over.
    pub fn cursor_right(&mut self) {
        if self.column_position < BUFFER_WIDTH {
            self.column_position += 1;
        }
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

lazy_static! {
    pub static ref WRITER: Mutex<Writer> = Mutex::new(Writer {
        column_position: 0,
        color_code: ColorCode::new(Color::LightCyan, Color::Black),
        buffer: unsafe {
            &mut *((PHYS_MEM_OFFSET.load(Ordering::Relaxed) + 0xb8000) as *mut Buffer)
        },
    });
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => ($crate::vga_buffer::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::kprint!("\n"));
    ($($arg:tt)*) => ($crate::kprint!("{}\n", format_args!($($arg)*)));
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    WRITER.lock().write_fmt(args).unwrap();
}

pub fn clear_screen() {
    WRITER.lock().clear_screen();
}

pub fn backspace() {
    WRITER.lock().backspace();
}

pub fn cursor_left() {
    WRITER.lock().cursor_left();
}

pub fn cursor_right() {
    WRITER.lock().cursor_right();
}
