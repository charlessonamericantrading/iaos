use lazy_static::lazy_static;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Index into the TSS's `privilege_stack_table` (RSP0/RSP1/RSP2) - only
/// RSP0 is meaningful for a kernel with no ring-1/ring-2 use: the CPU
/// loads it into RSP automatically on any privilege-level-raising
/// control transfer FROM ring 3 (an interrupt, exception, or the
/// `syscall` instruction while in user mode), the same way `interrupt_
/// stack_table`'s IST slots supply a stack for specific interrupts
/// regardless of the interrupted context's own (possibly corrupt or
/// unmapped) stack. Without this, ring-3 code could never be safely
/// interrupted at all - the CPU would try to use whatever RSP0 happens
/// to default to (zero), an immediate triple fault.
const RING3_RSP0_INDEX: usize = 0;

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            stack_start + STACK_SIZE as u64
        };
        tss.privilege_stack_table[RING3_RSP0_INDEX] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            stack_start + STACK_SIZE as u64
        };
        tss
    };
}

lazy_static! {
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        let kernel_code_selector = gdt.append(Descriptor::kernel_code_segment());
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));
        // Ring-3 (user-mode) code/data segments - the foundational piece
        // every real ring-3 transition needs before anything else. Laid
        // down here at Fase 68, before any of it was actually used -
        // Fase 69 added the first real DPL=3 IDT gate, Fase 70 the
        // first real USER-accessible page, and Fase 71 the actual
        // privilege-lowering control transfer (`ring3.rs`'s own
        // `iretq`-to-ring-3) - all three real, substantial pieces of
        // follow-on work this comment once described as not yet
        // started, shipped and exercised by dozens of self-tests since.
        // Appended in the conventional data-then-code order (matches
        // what `sysret`'s STAR MSR would expect, computing user SS/CS
        // as fixed offsets from a single base selector) even though
        // this Fase didn't use `syscall`/`sysret` yet - cheap to get
        // right at the time, expensive to silently rely on the wrong
        // order later.
        let user_data_selector = gdt.append(Descriptor::user_data_segment());
        let user_code_selector = gdt.append(Descriptor::user_code_segment());
        (
            gdt,
            Selectors {
                kernel_code_selector,
                tss_selector,
                user_data_selector,
                user_code_selector,
            },
        )
    };
}

struct Selectors {
    kernel_code_selector: SegmentSelector,
    tss_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
}

/// Real ring-3 selectors + the TSS's own RSP0, exposed for the self-test
/// (Fase 68) to verify this Fase's actual, checkable claims: both
/// selectors' DPL is genuinely Ring 3 (not just "some nonzero value"),
/// and RSP0 is a real, non-null kernel stack address. At the time this
/// was written, proof the GDT/TSS infrastructure was genuinely in
/// place (not merely compiled without erroring) was all that could be
/// claimed - proof ring-3 code can actually run came later, real and
/// substantial (Fase 71's own `ring3.rs`, now thousands of lines and
/// dozens of self-tests deep).
pub struct Ring3Info {
    pub user_code_selector: u16,
    pub user_data_selector: u16,
    pub user_code_rpl: u16,
    pub user_data_rpl: u16,
    pub rsp0: u64,
}

pub fn ring3_info() -> Ring3Info {
    Ring3Info {
        user_code_selector: GDT.1.user_code_selector.0,
        user_data_selector: GDT.1.user_data_selector.0,
        user_code_rpl: GDT.1.user_code_selector.0 & 0b11,
        user_data_rpl: GDT.1.user_data_selector.0 & 0b11,
        rsp0: TSS.privilege_stack_table[RING3_RSP0_INDEX].as_u64(),
    }
}

pub fn init() {
    use x86_64::instructions::segmentation::{Segment, CS, SS};
    use x86_64::instructions::tables::load_tss;

    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code_selector);
        // SS still holds whatever selector the bootloader's (now-replaced) GDT
        // used. Left alone, it stays a stale index that may alias an entry in
        // our new GDT (e.g. the TSS descriptor) and faults the next time the
        // CPU re-validates it, such as on the `iretq` from an interrupt. Data
        // segments are unused for addressing in long mode, so null is valid.
        SS::set_reg(SegmentSelector::NULL);
        load_tss(GDT.1.tss_selector);
    }
}
