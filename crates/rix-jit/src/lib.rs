//! `rix-jit` — W^X executable code-page allocator and audited VM runtime trampolines
//! for RixBrowser's Cranelift JIT engine (`rix-js`).
//!
//! Reference: Spec 05 §4.3 & Issue #19.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod memory;
pub mod trampolines;

pub use crate::memory::{
    live_code_bytes, live_code_pages, total_unmapped_pages, CodeAllocError, CodePage,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_page_alloc_wx_and_munmap_on_drop() {
        let before_pages = live_code_pages();
        let before_unmapped = total_unmapped_pages();
        {
            // Minimal valid NOP + RET bytes (never invoked; validates mmap RW -> mprotect RX -> munmap).
            let page = CodePage::from_machine_code(&[0x90, 0xC3]).expect("allocate W^X page");
            assert_eq!(page.code_len(), 2);
            assert!(live_code_pages() > before_pages);
        }
        assert_eq!(live_code_pages(), before_pages);
        assert!(total_unmapped_pages() > before_unmapped);
    }
}
