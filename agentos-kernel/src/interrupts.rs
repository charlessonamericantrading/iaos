use x86_64::structures::idt::InterruptDescriptorTable;
use lazy_static::lazy_static;
use crate::{kprintln, serial_println};

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let idt = InterruptDescriptorTable::new();
        idt
    };
}

pub fn init_idt() {
    IDT.load();
    kprintln!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
    serial_println!("[IDT] Native IDT Loaded into CPU Register (LIDT)");
}
