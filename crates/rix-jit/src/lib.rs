//! `rix-jit` — Multi-Tier Cranelift JIT Engine (`Tier-2 Baseline/OSR` + `Tier-3 Speculative
//! Optimizing JIT`) for RixBrowser's JavaScript VM (`rix-js`).
//!
//! Reference: Spec 05 §4.3, §4.4 & Issues #19–#20.
//!
//! Safety discipline:
//! - Crate-wide `#![deny(unsafe_op_in_unsafe_fn)]`.
//! - Every `unsafe` block is localized to W^X `mmap`/`mprotect`/`munmap` management
//!   in [`memory`], native entry invocation in [`baseline`]/[`optimizer`], and `extern "C"`
//!   VM trampolines in [`trampolines`], each documented with an explicit `// SAFETY:` proof.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod baseline;
pub mod memory;
pub mod opt_analysis;
pub mod optimizer;
pub mod trampolines;
pub mod wasm;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rix_js::{
    JitEngine, JitOutcome, JsValue, JsVm, BASELINE_THRESHOLD, OPT_THRESHOLD, OSR_THRESHOLD,
};

use crate::trampolines::{DeoptReason, JIT_DEOPT_TAG, JIT_EXCEPTION_SENTINEL};

pub use crate::baseline::{compile_baseline_function, CompiledBaseline, JitCompileError};
pub use crate::memory::{
    live_code_bytes, live_code_pages, total_unmapped_pages, CodeAllocError, CodePage,
};
pub use crate::optimizer::{
    compile_optimizing_function, CompiledOptimized, DeoptMetadata, OptHints,
};
pub use crate::wasm::{install_webassembly, SharedWasmHost, WasmHost};

/// Maximum Tier-3 speculative deoptimizations per function before the deopt-loop circuit
/// breaker permanently demotes that function to Tier-2 Baseline.
pub const MAX_OPT_DEOPTS: u32 = 4;

/// Runtime compilation, tier-up, and execution counters for [`BaselineJit`] / [`TieredJit`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitStats {
    pub compiled_functions: u64,
    pub unsupported_functions: u64,
    pub baseline_entries: u64,
    pub osr_entries: u64,
    pub deopt_exits: u64,
    pub exception_exits: u64,
    pub opt_compiled_functions: u64,
    pub opt_entries: u64,
    pub opt_deopts: u64,
    pub circuit_breaker_trips: u64,
    pub hoisted_shape_guards: u64,
    pub hoisted_invariant_loads: u64,
    pub hoisted_array_guards: u64,
    pub inlined_call_sites: u64,
}

enum CompiledSlot {
    Compiled(Arc<CompiledBaseline>),
    Unsupported,
}

enum OptSlot {
    Compiled(Arc<CompiledOptimized>),
    Disabled,
}

#[derive(Debug, Clone, Copy, Default)]
struct FuncOptMeta {
    hints: OptHints,
    deopt_count: u32,
    last_deopt_reason: Option<DeoptReason>,
}

/// Multi-tier Cranelift JIT backend (`Tier-2 Baseline + OSR` and `Tier-3 Speculative Optimizing`)
/// implementing [`rix_js::JitEngine`].
///
/// Owns all compiled W^X [`CodePage`] allocations for the attached realm; dropping
/// the realm or calling [`BaselineJit::clear_cache`] deterministically unmaps every
/// executable page via `libc::munmap`.
pub struct BaselineJit {
    baseline_cache: RefCell<HashMap<(u32, u32), CompiledSlot>>,
    opt_cache: RefCell<HashMap<(u32, u32), OptSlot>>,
    opt_meta: RefCell<HashMap<(u32, u32), FuncOptMeta>>,
    baseline_threshold: u32,
    osr_threshold: u32,
    opt_threshold: u32,
    stats: RefCell<JitStats>,
    last_error: RefCell<Option<String>>,
}

/// Canonical alias highlighting full Tier-2 + Tier-3 multi-tier compilation.
pub type TieredJit = BaselineJit;

impl BaselineJit {
    /// Creates a new tiered JIT using specification thresholds
    /// (`BASELINE_THRESHOLD = 32`, `OSR_THRESHOLD = 256`, `OPT_THRESHOLD = 1_000`).
    #[must_use]
    pub fn new() -> Rc<Self> {
        Self::with_tier_thresholds(BASELINE_THRESHOLD, OSR_THRESHOLD, OPT_THRESHOLD)
    }

    /// Creates a JIT with custom Tier-2 Baseline/OSR thresholds (retaining default `OPT_THRESHOLD`).
    #[must_use]
    pub fn with_thresholds(baseline_threshold: u32, osr_threshold: u32) -> Rc<Self> {
        Self::with_tier_thresholds(baseline_threshold, osr_threshold, OPT_THRESHOLD)
    }

    /// Creates a JIT with custom Tier-2 Baseline, OSR, and Tier-3 Optimizing thresholds
    /// (e.g. `(1, 1, 1)` for exhaustive Tier-3 Test262 conformance verification).
    #[must_use]
    pub fn with_tier_thresholds(
        baseline_threshold: u32,
        osr_threshold: u32,
        opt_threshold: u32,
    ) -> Rc<Self> {
        Rc::new(Self {
            baseline_cache: RefCell::new(HashMap::new()),
            opt_cache: RefCell::new(HashMap::new()),
            opt_meta: RefCell::new(HashMap::new()),
            baseline_threshold,
            osr_threshold,
            opt_threshold,
            stats: RefCell::new(JitStats::default()),
            last_error: RefCell::new(None),
        })
    }

    /// Creates and attaches a default multi-tier JIT to `vm`.
    pub fn attach_to(vm: &mut JsVm) -> Rc<Self> {
        let jit = Self::new();
        vm.attach_jit(jit.clone());
        jit
    }

    /// Returns a snapshot of compilation, tier-up, and execution telemetry.
    #[must_use]
    pub fn stats(&self) -> JitStats {
        *self.stats.borrow()
    }

    /// Returns the most recent compilation skip/error reason, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.borrow().clone()
    }

    /// Returns the total native machine code bytes currently cached across Tier-2 and Tier-3.
    #[must_use]
    pub fn compiled_code_bytes(&self) -> usize {
        let t2: usize = self
            .baseline_cache
            .borrow()
            .values()
            .map(|slot| match slot {
                CompiledSlot::Compiled(c) => c.code_len(),
                CompiledSlot::Unsupported => 0,
            })
            .sum();
        let t3: usize = self
            .opt_cache
            .borrow()
            .values()
            .map(|slot| match slot {
                OptSlot::Compiled(c) => c.code_len(),
                OptSlot::Disabled => 0,
            })
            .sum();
        t2 + t3
    }

    /// Returns the deoptimization count recorded for `(program_id, func_idx)`.
    #[must_use]
    pub fn func_deopt_count(&self, program_id: u32, func_idx: u32) -> u32 {
        self.opt_meta
            .borrow()
            .get(&(program_id, func_idx))
            .map_or(0, |m| m.deopt_count)
    }

    /// Drops all cached Tier-2 and Tier-3 code artifacts, triggering `munmap` once no active
    /// native frame holds an `Arc`.
    pub fn clear_cache(&self) {
        self.baseline_cache.borrow_mut().clear();
        self.opt_cache.borrow_mut().clear();
    }

    fn get_or_compile_baseline(
        &self,
        vm: &JsVm,
        program_id: u32,
        func_idx: u32,
    ) -> Option<Arc<CompiledBaseline>> {
        let key = (program_id, func_idx);
        if let Some(slot) = self.baseline_cache.borrow().get(&key) {
            return match slot {
                CompiledSlot::Compiled(c) => Some(Arc::clone(c)),
                CompiledSlot::Unsupported => None,
            };
        }

        let func = vm.function_bytecode(program_id, func_idx)?;
        match compile_baseline_function(program_id, func_idx, func) {
            Ok(compiled) => {
                self.stats.borrow_mut().compiled_functions += 1;
                self.baseline_cache
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
                self.baseline_cache
                    .borrow_mut()
                    .insert(key, CompiledSlot::Unsupported);
                None
            }
        }
    }

    fn get_or_compile_opt(
        &self,
        vm: &JsVm,
        program_id: u32,
        func_idx: u32,
    ) -> Option<Arc<CompiledOptimized>> {
        let key = (program_id, func_idx);
        if let Some(slot) = self.opt_cache.borrow().get(&key) {
            return match slot {
                OptSlot::Compiled(c) => Some(Arc::clone(c)),
                OptSlot::Disabled => None,
            };
        }

        let hints = self
            .opt_meta
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or_default()
            .hints;
        let func = vm.function_bytecode(program_id, func_idx)?;
        match compile_optimizing_function(vm, program_id, func_idx, func, hints) {
            Ok(compiled) => {
                {
                    let mut st = self.stats.borrow_mut();
                    st.opt_compiled_functions += 1;
                    st.hoisted_shape_guards += compiled.hoisted_shape_guards as u64;
                    st.hoisted_invariant_loads += compiled.hoisted_invariant_loads as u64;
                    st.hoisted_array_guards += compiled.hoisted_array_guards as u64;
                    st.inlined_call_sites += compiled.inlined_call_sites as u64;
                }
                self.opt_cache
                    .borrow_mut()
                    .insert(key, OptSlot::Compiled(Arc::clone(&compiled)));
                Some(compiled)
            }
            Err(err) => {
                *self.last_error.borrow_mut() =
                    Some(format!("tier3 prog={program_id} func={func_idx}: {err}"));
                tracing::debug!(
                    program_id,
                    func_idx,
                    reason = %err,
                    "Tier-3 Optimizing JIT skipped function; staying on Tier-2 Baseline"
                );
                self.opt_cache.borrow_mut().insert(key, OptSlot::Disabled);
                None
            }
        }
    }

    fn handle_opt_deopt(&self, program_id: u32, func_idx: u32, reason: DeoptReason) {
        let key = (program_id, func_idx);
        let mut meta_map = self.opt_meta.borrow_mut();
        let meta = meta_map.entry(key).or_default();
        meta.deopt_count = meta.deopt_count.saturating_add(1);
        meta.last_deopt_reason = Some(reason);

        {
            let mut st = self.stats.borrow_mut();
            st.deopt_exits += 1;
            st.opt_deopts += 1;
        }

        if meta.deopt_count >= MAX_OPT_DEOPTS {
            // Deopt-loop circuit breaker: permanently demote to Tier-2 Baseline and unmap Tier-3 page.
            self.opt_cache.borrow_mut().insert(key, OptSlot::Disabled);
            self.stats.borrow_mut().circuit_breaker_trips += 1;
        } else {
            // Adaptively disable the failing speculation class and invalidate so next hot call
            // recompiles cleanly with updated IC feedback.
            match reason {
                DeoptReason::Int32Type | DeoptReason::Int32Overflow => {
                    meta.hints.speculate_int32 = false;
                }
                DeoptReason::ShapeGuard | DeoptReason::PrototypeEpoch => {
                    meta.hints.speculate_shapes = false;
                }
                DeoptReason::ArrayGuard | DeoptReason::ArrayBounds => {
                    meta.hints.speculate_arrays = false;
                }
                DeoptReason::CalleeGuard => {
                    meta.hints.inline_calls = false;
                }
            }
            self.opt_cache.borrow_mut().remove(&key);
        }
    }
}

impl JitEngine for BaselineJit {
    fn baseline_threshold(&self) -> u32 {
        self.baseline_threshold.min(self.opt_threshold)
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
        // Tier-3 Speculative Optimizing JIT entry when `entry_pc == 0` and call count >= `opt_threshold`.
        if entry_pc == 0 && call_count >= self.opt_threshold {
            if let Some(opt) = self.get_or_compile_opt(vm, program_id, func_idx) {
                let base = vm.jit_current_frame_base();
                let regs_slice = vm.jit_registers_from_mut(base);
                if regs_slice.len() >= opt.reg_count {
                    let regs_ptr = regs_slice.as_mut_ptr().cast::<u64>();
                    let safepoint_mask: u32 = if vm.max_loop_iterations.is_some() {
                        0
                    } else {
                        0xFF
                    };
                    self.stats.borrow_mut().opt_entries += 1;

                    // SAFETY:
                    // - `vm` is exclusively borrowed for this call with matching top frame.
                    // - `regs_ptr` has `>= opt.reg_count` valid `JsValue`-sized `u64` slots.
                    // - `opt` (`Arc<CompiledOptimized>`) remains alive on stack across execution.
                    let raw =
                        unsafe { opt.invoke(std::ptr::from_mut(vm), regs_ptr, safepoint_mask) };

                    if raw == JIT_EXCEPTION_SENTINEL {
                        self.stats.borrow_mut().exception_exits += 1;
                        let exc = vm
                            .pending_jit_exception
                            .take()
                            .unwrap_or_else(|| vm.type_error("Unhandled Tier-3 JIT exception"));
                        return JitOutcome::Threw(exc);
                    }
                    if let Some((reason, resume_pc)) = DeoptReason::from_raw(raw) {
                        self.handle_opt_deopt(program_id, func_idx, reason);
                        return JitOutcome::Deopt { resume_pc };
                    }
                    return JitOutcome::Returned(JsValue::from_raw(raw));
                }
            }
        }

        // Tier-2 Baseline Cranelift JIT & OSR entry.
        if entry_pc == 0 {
            if call_count < self.baseline_threshold && back_edge_count < self.osr_threshold {
                return JitOutcome::Fallback;
            }
        } else if back_edge_count < self.osr_threshold {
            return JitOutcome::Fallback;
        }

        let Some(compiled) = self.get_or_compile_baseline(vm, program_id, func_idx) else {
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
        // - `vm` is exclusively borrowed for this call with matching top frame.
        // - `regs_ptr` has `>= compiled.reg_count` valid `JsValue`-sized `u64` slots.
        // - `compiled` (`Arc<CompiledBaseline>`) remains alive on stack across execution.
        // - `compiled.supports_entry(entry_pc)` was verified above.
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
