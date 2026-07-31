use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::registers::control::Cr2;
use lazy_static::lazy_static;
use crate::{kprintln, serial_println, gdt};

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
        idt
    };
}

pub fn init_idt() {
    IDT.load();
    kprintln!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
    serial_println!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
    kprintln!("[IDT] Handlers armed: breakpoint, double-fault(IST), page-fault, GPF, divide-error, invalid-opcode");
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
