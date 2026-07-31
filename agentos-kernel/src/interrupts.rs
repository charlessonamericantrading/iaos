use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::registers::control::Cr2;
use lazy_static::lazy_static;
use spin::Mutex;
use pic8259::ChainedPics;
use crate::{kprintln, serial_println, gdt};

/// The legacy 8259 PIC defaults to delivering IRQ0-7 on vectors 0x08-0x0F,
/// which collide head-on with CPU exceptions (0x08 is literally our
/// double-fault vector). Remapping both PICs to start at 0x20 (32) is what
/// makes it safe to ever call `interrupts::enable()` - without this, the
/// very first timer tick would look like a double fault to the CPU.
pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault.set_handler_fn(general_protection_fault_handler);
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
        idt
    };
}

pub fn init_idt() {
    IDT.load();
    kprintln!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
    serial_println!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
    kprintln!("[IDT] Handlers armed: breakpoint, double-fault(IST), page-fault, GPF, divide-error, invalid-opcode, timer(IRQ0), keyboard(IRQ1)");
}

/// Remaps the 8259 PIC to `PIC_1_OFFSET..PIC_2_OFFSET+8` and unmasks IRQs.
/// Must run after `init_idt()` (so vectors 32/33 already have handlers) and
/// before `x86_64::instructions::interrupts::enable()`.
pub fn init_pics() {
    unsafe { PICS.lock().initialize() };
    kprintln!("[PIC] 8259 remapped to vectors {}-{} (was 8-15, collided with CPU exceptions)", PIC_1_OFFSET, PIC_2_OFFSET + 7);
    serial_println!("[PIC] 8259 remapped to vectors {}-{}", PIC_1_OFFSET, PIC_2_OFFSET + 7);
}

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe {
        PICS.lock().notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use x86_64::instructions::port::Port;

    let mut data_port: Port<u8> = Port::new(0x60);
    let scancode: u8 = unsafe { data_port.read() };
    crate::keyboard::handle_scancode(scancode);

    unsafe {
        PICS.lock().notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}

/// Non-fatal: a debugger/test breakpoint (int3). Execution continues after this.
extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    kprintln!("[EXCEPTION] Breakpoint at {:#x}", stack_frame.instruction_pointer.as_u64());
    serial_println!("[EXCEPTION] Breakpoint\n{:#?}", stack_frame);
}

/// Fatal: only fires when a second exception occurs while handling the first
/// (e.g. a page-fault handler itself faults). Runs on its own IST stack
/// (see gdt.rs) because the normal stack may already be corrupt at this point.
extern "x86-interrupt" fn double_fault_handler(stack_frame: InterruptStackFrame, _error_code: u64) -> ! {
    serial_println!("[FATAL] DOUBLE FAULT\n{:#?}", stack_frame);
    kprintln!("[FATAL] DOUBLE FAULT - Kernel Halted");
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn page_fault_handler(stack_frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let fault_addr = Cr2::read();
    kprintln!("[EXCEPTION] Page Fault accessing {:?}", fault_addr);
    serial_println!("[EXCEPTION] Page Fault at {:?}, code {:?}\n{:#?}", fault_addr, error_code, stack_frame);
    kprintln!("[EXCEPTION] Kernel Halted (no recovery yet - see Fase 4 paging work)");
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn general_protection_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    kprintln!("[EXCEPTION] General Protection Fault, error code {}", error_code);
    serial_println!("[EXCEPTION] GPF code {}\n{:#?}", error_code, stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn divide_error_handler(stack_frame: InterruptStackFrame) {
    kprintln!("[EXCEPTION] Divide Error (division by zero)");
    serial_println!("[EXCEPTION] Divide Error\n{:#?}", stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    kprintln!("[EXCEPTION] Invalid Opcode at {:#x}", stack_frame.instruction_pointer.as_u64());
    serial_println!("[EXCEPTION] Invalid Opcode\n{:#?}", stack_frame);
    loop {
        x86_64::instructions::hlt();
    }
}
