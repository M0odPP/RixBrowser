//! `rix-jit` — Tier-2 Baseline Cranelift JIT & On-Stack Replacement (OSR) engine
//! for RixBrowser's JavaScript VM (`rix-js`).
//!
//! Reference: Spec 05 §4.3 & Issue #19.
//!
//! Safety discipline:
//! - Crate-wide `#![deny(unsafe_op_in_unsafe_fn)]`.
//! - Every `unsafe` block is localized to W^X `mmap`/`mprotect`/`munmap` management
//!   in [`memory`], native entry invocation in [`baseline`], and `extern "C"` VM
//!   trampolines in [`trampolines`], each documented with an explicit `// SAFETY:` proof.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod baseline;
pub mod memory;
pub mod opt_analysis;
pub mod trampolines;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_js::{JitEngine, JitOutcome, JsValue, JsVm, BASELINE_THRESHOLD, OSR_THRESHOLD};

use crate::trampolines::{JIT_DEOPT_TAG, JIT_EXCEPTION_SENTINEL};

pub use crate::baseline::{compile_baseline_function, CompiledBaseline, JitCompileError};
pub use crate::memory::{
    live_code_bytes, live_code_pages, total_unmapped_pages, CodeAllocError, CodePage,
};

/// Runtime compilation and execution counters for [`BaselineJit`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitStats {
    pub compiled_functions: u64,
    pub unsupported_functions: u64,
    pub baseline_entries: u64,
    pub osr_entries: u64,
    pub deopt_exits: u64,
    pub exception_exits: u64,
}

enum CompiledSlot {
    Compiled(Arc<CompiledBaseline>),
    Unsupported,
}

/// Tier-2 Baseline Cranelift JIT & OSR backend implementing [`rix_js::JitEngine`].
///
/// Owns all compiled W^X [`CodePage`] allocations for the attached realm; dropping
/// the realm or calling [`BaselineJit::clear_cache`] deterministically unmaps every
/// executable page via `libc::munmap`.
pub struct BaselineJit {
    cache: RefCell<HashMap<(u32, u32), CompiledSlot>>,
    baseline_threshold: u32,
    osr_threshold: u32,
    stats: RefCell<JitStats>,
    last_error: RefCell<Option<String>>,
}

impl BaselineJit {
    /// Creates a new [`BaselineJit`] using the specification thresholds
    /// (`BASELINE_THRESHOLD = 32`, `OSR_THRESHOLD = 256`).
    #[must_use]
    pub fn new() -> Rc<Self> {
        Self::with_thresholds(BASELINE_THRESHOLD, OSR_THRESHOLD)
    }

    /// Creates a [`BaselineJit`] with custom hotness thresholds (e.g. `(1, 1)` for
    /// exhaustive Test262 parity verification).
    #[must_use]
    pub fn with_thresholds(baseline_threshold: u32, osr_threshold: u32) -> Rc<Self> {
        Rc::new(Self {
            cache: RefCell::new(HashMap::new()),
            baseline_threshold,
            osr_threshold,
            stats: RefCell::new(JitStats::default()),
            last_error: RefCell::new(None),
        })
    }

    /// Creates and attaches a default [`BaselineJit`] to `vm`, returning a handle for
    /// inspecting JIT telemetry.
    pub fn attach_to(vm: &mut JsVm) -> Rc<Self> {
        let jit = Self::new();
        vm.attach_jit(jit.clone());
        jit
    }

    /// Returns a snapshot of compilation and execution telemetry.
    #[must_use]
    pub fn stats(&self) -> JitStats {
        *self.stats.borrow()
    }

    /// Returns the most recent compilation skip/error reason, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.borrow().clone()
    }

    /// Returns the total native machine code bytes currently cached by this instance.
    #[must_use]
    pub fn compiled_code_bytes(&self) -> usize {
        self.cache
            .borrow()
            .values()
            .map(|slot| match slot {
                CompiledSlot::Compiled(c) => c.code_len(),
                CompiledSlot::Unsupported => 0,
            })
            .sum()
    }

    /// Drops all cached compiled code artifacts, triggering `munmap` once no active
    /// native frame holds an `Arc<CompiledBaseline>`.
    pub fn clear_cache(&self) {
        self.cache.borrow_mut().clear();
    }

    fn get_or_compile(
        &self,
        vm: &JsVm,
        program_id: u32,
        func_idx: u32,
    ) -> Option<Arc<CompiledBaseline>> {
        let key = (program_id, func_idx);
        if let Some(slot) = self.cache.borrow().get(&key) {
            return match slot {
                CompiledSlot::Compiled(c) => Some(Arc::clone(c)),
                CompiledSlot::Unsupported => None,
            };
        }

        let func = vm.function_bytecode(program_id, func_idx)?;
        match compile_baseline_function(program_id, func_idx, func) {
            Ok(compiled) => {
                self.stats.borrow_mut().compiled_functions += 1;
                self.cache
                    .borrow_mut()
                    .insert(key, CompiledSlot::Compiled(Arc::clone(&compiled)));
                Some(compiled)
            }
            Err(err) => {
                *self.last_error.borrow_mut() =
                    Some(format!("prog={program_id} func={func_idx}: {err}"));
                tracing::debug!(
                    program_id,
                    func_idx,
                    reason = %err,
                    "Baseline JIT skipped function; falling back to Tier-1 interpreter"
                );
                self.stats.borrow_mut().unsupported_functions += 1;
                self.cache
                    .borrow_mut()
                    .insert(key, CompiledSlot::Unsupported);
                None
            }
        }
    }
}

impl JitEngine for BaselineJit {
    fn baseline_threshold(&self) -> u32 {
        self.baseline_threshold
    }

    fn osr_threshold(&self) -> u32 {
        self.osr_threshold
    }

    fn try_enter(
        &self,
        vm: &mut JsVm,
        program_id: u32,
        func_idx: u32,
        entry_pc: usize,
        call_count: u32,
        back_edge_count: u32,
    ) -> JitOutcome {
        if entry_pc == 0 {
            if call_count < self.baseline_threshold && back_edge_count < self.osr_threshold {
                return JitOutcome::Fallback;
            }
        } else if back_edge_count < self.osr_threshold {
            return JitOutcome::Fallback;
        }

        // Borrow `self.cache` only during lookup/compilation; release before invoking
        // native code so recursive JS-to-JS calls can re-enter `try_enter`.
        let Some(compiled) = self.get_or_compile(vm, program_id, func_idx) else {
            return JitOutcome::Fallback;
        };
        if !compiled.supports_entry(entry_pc) {
            return JitOutcome::Fallback;
        }

        let base = vm.jit_current_frame_base();
        let regs_slice = vm.jit_registers_from_mut(base);
        if regs_slice.len() < compiled.reg_count {
            return JitOutcome::Fallback;
        }
        let regs_ptr = regs_slice.as_mut_ptr().cast::<u64>();

        // When an explicit loop budget is installed, run the safepoint check on every
        // back-edge (`mask = 0`) so tight budgets interrupt deterministically.
        // Otherwise amortize cooperative GC/interrupt checks to every 256 iterations.
        let safepoint_mask: u32 = if vm.max_loop_iterations.is_some() {
            0
        } else {
            0xFF
        };

        {
            let mut st = self.stats.borrow_mut();
            if entry_pc == 0 {
                st.baseline_entries += 1;
            } else {
                st.osr_entries += 1;
            }
        }

        // SAFETY:
        // - `vm` is exclusively borrowed for the duration of this call and its top frame
        //   matches `(program_id, func_idx)`.
        // - `regs_ptr` points to `>= compiled.reg_count` valid `JsValue` (layout-identical
        //   `u64`) slots in `vm.registers[base..]`.
        // - `compiled` (`Arc<CompiledBaseline>`) stays alive on the stack for the entire
        //   native invocation even if the cache is mutated re-entrantly.
        // - `compiled.supports_entry(entry_pc)` was verified immediately above.
        let raw = unsafe {
            compiled.invoke(
                std::ptr::from_mut(vm),
                regs_ptr,
                entry_pc as u32,
                safepoint_mask,
            )
        };

        if raw == JIT_EXCEPTION_SENTINEL {
            self.stats.borrow_mut().exception_exits += 1;
            let exc = vm
                .pending_jit_exception
                .take()
                .unwrap_or_else(|| vm.type_error("Unhandled JIT exception"));
            return JitOutcome::Threw(exc);
        }

        if (raw & 0xFFFF_0000_0000_0000) == JIT_DEOPT_TAG {
            self.stats.borrow_mut().deopt_exits += 1;
            return JitOutcome::Deopt {
                resume_pc: (raw & 0xFFFF_FFFF) as usize,
            };
        }

        JitOutcome::Returned(JsValue::from_raw(raw))
    }
}
