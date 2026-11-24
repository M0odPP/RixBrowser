//! Tier-3 Speculative Optimizing Cranelift JIT Compiler.
//!
//! Reference: Spec 05 §4.4, §6 & Issue #20.
//!
//! Triggered at `OPT_THRESHOLD = 1_000` calls, consuming monomorphic Inline Cache
//! (`IcState`), call-site (`CallSiteFeedback`), and array `ElementsKind` feedback:
//! - **Hoisted Shape Guards & Direct Slot Access:** Validates `obj.shape_id` once at
//!   pure loop preheaders (and coalesces repeated guards per basic block outside loops),
//!   turning `GetProp`/`SetProp` into single native `[slots_ptr + offset*8]` CPU loads/stores.
//! - **Loop-Invariant Property Load Hoisting (LICM):** Own-data properties never mutated
//!   inside a pure loop are loaded once at the loop preheader into CPU registers.
//! - **Prototype-Chain Epoch Guards:** Validates `ShapeTable::prototype_epoch()` + receiver
//!   shape, deoptimizing immediately if any prototype object in the realm mutates.
//! - **Unboxed `Int32` Speculation & Packed Array Loop Hoisting:** Specializes hot arithmetic
//!   and `PackedInt32`/`PackedFloat64`/`PackedValues` array loops (including hoisted `arr.length`)
//!   with exact overflow and bounds deoptimization back to bytecode PC.
//! - **Small Pure Leaf Callee Inlining:** Inlines monomorphic pure leaf JS functions directly
//!   into the caller's Cranelift SSA IR behind a 1-instruction callee identity guard.
//! - **Exact Deoptimization Metadata:** Every speculative guard failure materializes all live
//!   SSA registers to `vm.registers[base..]` and returns `DeoptReason::encode_deopt(resume_pc)`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, AbiParam, Block, Function, InstBuilder, MemFlagsData, Signature, StackSlot,
    StackSlotData, StackSlotKind, UserFuncName, Value,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use rix_js::{ConstantEntry, ElementsKind, FunctionBytecode, IcState, JsValue, JsVm, Opcode};

use crate::baseline::{BaselineEntryFn, JitCompileError};
use crate::memory::CodePage;
use crate::trampolines::{
    rt_add_slow, rt_call_fast2, rt_cmp_slow, rt_div_slow, rt_get_elem, rt_get_elem_fast,
    rt_get_global_ic_fast, rt_get_prop_ic, rt_get_prop_ic_fast, rt_guard_object_slots,
    rt_guard_packed_array, rt_guard_proto_prop, rt_load_const, rt_load_this, rt_loop_safepoint,
    rt_mul_slow, rt_regs_ptr, rt_rem_slow, rt_set_elem, rt_set_elem_fast, rt_set_prop_ic,
    rt_set_prop_ic_fast, rt_step_generic, rt_sub_slow, rt_throw, rt_to_bool, rt_write_barrier,
    DeoptReason, JIT_EXCEPTION_SENTINEL, JIT_MISS_SENTINEL,
};

const TAG_MASK: i64 = 0xFFFF_0000_0000_0000u64 as i64;
const PAYLOAD_32_MASK: i64 = 0x0000_0000_FFFF_FFFF;
const TAG_THRESHOLD: i64 = 0xFFF8_0000_0000_0000u64 as i64;
const TAG_INT32: i64 = 0xFFF9_0000_0000_0000u64 as i64;
const TAG_SPECIAL: i64 = 0xFFFA_0000_0000_0000u64 as i64;
const TAG_STRING_MIN: i64 = 0xFFFB_0000_0000_0000u64 as i64;
const TAG_OBJECT: i64 = 0xFFFC_0000_0000_0000u64 as i64;
const CANONICAL_NAN: i64 = rix_js::CANONICAL_NAN_BITS as i64;
const VAL_UNDEFINED: i64 = JsValue::UNDEFINED.to_raw() as i64;
const VAL_NULL: i64 = JsValue::NULL.to_raw() as i64;
const VAL_FALSE: i64 = JsValue::FALSE.to_raw() as i64;
const VAL_TRUE: i64 = JsValue::TRUE.to_raw() as i64;

use crate::opt_analysis::{
    advance_pc, analyze_loops, check_inline_candidate, check_opt_eligibility, is_length_ic,
    resolve_wide, scan_block_object_requirements, InlineCandidate, LoopOptPlan,
};
pub use crate::opt_analysis::{DeoptMetadata, OptHints};

/// Finalized Tier-3 Optimizing JIT native artifact owning its W^X [`CodePage`].
pub struct CompiledOptimized {
    code_page: CodePage,
    pub reg_count: usize,
    pub deopt_sites: Vec<DeoptMetadata>,
    pub hoisted_shape_guards: usize,
    pub hoisted_invariant_loads: usize,
    pub hoisted_array_guards: usize,
    pub inlined_call_sites: usize,
}

impl CompiledOptimized {
    #[must_use]
    pub fn code_len(&self) -> usize {
        self.code_page.code_len()
    }

    /// Invokes the finalized Tier-3 native code at function entry (`entry_pc = 0`).
    ///
    /// # Safety
    /// - `vm` must be a valid active `JsVm` pointer whose top frame matches the compiled function.
    /// - `regs_ptr` must point to at least `self.reg_count` readable/writable `u64` register slots.
    pub unsafe fn invoke(&self, vm: *mut JsVm, regs_ptr: *mut u64, safepoint_mask: u32) -> u64 {
        let raw_ptr = self.code_page.as_ptr();
        // SAFETY: `self.code_page` holds finalized RX machine code emitted by Cranelift for the
        // host ISA matching `BaselineEntryFn`, and stays alive for `&self`.
        let entry: BaselineEntryFn = unsafe { std::mem::transmute(raw_ptr) };
        // SAFETY: Caller upholds `vm` and `regs_ptr` invariants; Tier-3 always enters at PC 0.
        unsafe { entry(vm, regs_ptr, 0, safepoint_mask) }
    }
}

/// Compiles `(program_id, func_idx)` using Tier-3 speculative optimizations guided by live VM
/// Inline Cache, call-site, and array elements-kind feedback.
pub fn compile_optimizing_function(
    vm: &JsVm,
    program_id: u32,
    func_idx: u32,
    func: &FunctionBytecode,
    hints: OptHints,
) -> Result<Arc<CompiledOptimized>, JitCompileError> {
    check_opt_eligibility(func)?;

    let ics = vm.jit_ic_snapshot(program_id, func_idx);
    let current_proto_epoch = vm.shapes.prototype_epoch();

    // Pre-discover eligible small leaf inline call sites.
    let mut inline_map: HashMap<usize, InlineCandidate> = HashMap::new();
    if hints.inline_calls {
        let mut spc = 0usize;
        while spc < func.code.len() {
            let inst = func.code[spc];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(func, spc, op);
            if op == Opcode::Call {
                let argc = usize::from(inst.c());
                let fb = vm.jit_call_feedback(program_id, func_idx, spc);
                if let Some(cand) = check_inline_candidate(vm, fb, argc) {
                    // Never recursively inline a function into itself.
                    if !(cand.program_id == program_id && cand.func_idx == func_idx) {
                        inline_map.insert(spc, cand);
                    }
                }
            }
            spc = next_pc;
        }
    }

    let has_closures = func.code.iter().any(|i| {
        matches!(
            i.opcode(),
            Some(Opcode::NewClosure | Opcode::SetUpvalue | Opcode::GetUpvalue)
        )
    });

    // Analyze natural loops for hoisted shape guards, LICM loads, and hoisted packed-array loops.
    let (loop_plans, all_loop_ranges) = analyze_loops(
        vm,
        (program_id, func_idx),
        func,
        &ics,
        &inline_map,
        has_closures,
        hints,
    );

    let mut flag_builder = settings::builder();
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| JitCompileError::Cranelift(e.to_string()))?;
    flag_builder
        .set("is_pic", "true")
        .map_err(|e| JitCompileError::Cranelift(e.to_string()))?;
    let flags = settings::Flags::new(flag_builder);
    let isa = cranelift_native::builder()
        .map_err(|e| JitCompileError::UnsupportedIsa(e.to_string()))?
        .finish(flags)
        .map_err(|e| JitCompileError::UnsupportedIsa(e.to_string()))?;

    let ptr_ty = isa.pointer_type();
    let call_conv = CallConv::triple_default(isa.triple());

    let mut entry_sig = Signature::new(call_conv);
    entry_sig.params.push(AbiParam::new(ptr_ty));
    entry_sig.params.push(AbiParam::new(ptr_ty));
    entry_sig.params.push(AbiParam::new(types::I32));
    entry_sig.params.push(AbiParam::new(types::I32));
    entry_sig.returns.push(AbiParam::new(types::I64));

    let mut clif_func =
        Function::with_name_signature(UserFuncName::user(program_id, func_idx), entry_sig);
    let mut fb_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut clif_func, &mut fb_ctx);

    let reg_count = (func.register_count as usize).max(func.param_count as usize + 1);

    // Pre-scan all basic block leaders.
    let mut leaders = BTreeSet::new();
    leaders.insert(0usize);
    let mut scan_pc = 0usize;
    while scan_pc < func.code.len() {
        let inst = func.code[scan_pc];
        let op = inst.opcode().unwrap_or(Opcode::Nop);
        let next_pc = advance_pc(func, scan_pc, op);
        match op {
            Opcode::Jump
            | Opcode::JumpIfFalse
            | Opcode::JumpIfTrue
            | Opcode::JumpIfNullish
            | Opcode::JumpIfNotUndef
            | Opcode::LoopBack => {
                let target = ((scan_pc + 1) as isize + isize::from(inst.sbx())) as usize;
                leaders.insert(target);
                leaders.insert(next_pc);
            }
            Opcode::Return | Opcode::Throw => {
                leaders.insert(next_pc);
            }
            _ => {}
        }
        scan_pc = next_pc;
    }

    let prologue = builder.create_block();
    builder.append_block_params_for_function_params(prologue);

    let mut pc_blocks: HashMap<usize, Block> = HashMap::new();
    let mut loop_body_blocks: HashMap<usize, Block> = HashMap::new();
    for &lpc in &leaders {
        if lpc < func.code.len() {
            pc_blocks.insert(lpc, builder.create_block());
            if loop_plans.contains_key(&lpc) {
                loop_body_blocks.insert(lpc, builder.create_block());
            }
        }
    }
    let implicit_ret_block = builder.create_block();
    let exc_exit_block = builder.create_block();

    // Declare SSA Variables for bytecode registers, live `regs_ptr`, and loop safepoint counter.
    let reg_vars: Vec<Variable> = (0..reg_count)
        .map(|_| builder.declare_var(types::I64))
        .collect();
    let regs_ptr_var = builder.declare_var(ptr_ty);
    let loop_tick_var = builder.declare_var(types::I32);

    // Declare per-loop hoisted SSA Variables for object `slots_ptr`, LICM property values,
    // and packed-array `(arr_ptr, arr_len)`.
    let mut loop_obj_ptr_vars: HashMap<(usize, usize), Variable> = HashMap::new();
    let mut loop_licm_vars: HashMap<(usize, usize, u32), Variable> = HashMap::new();
    let mut loop_arr_vars: HashMap<(usize, usize), (Variable, Variable, ElementsKind)> =
        HashMap::new();

    let mut hoisted_shape_guards = 0usize;
    let mut hoisted_invariant_loads = 0usize;
    let mut hoisted_array_guards = 0usize;

    for (&hdr, plan) in &loop_plans {
        for (&obj_r, oplan) in &plan.hoisted_objects {
            let pvar = builder.declare_var(ptr_ty);
            loop_obj_ptr_vars.insert((hdr, obj_r), pvar);
            hoisted_shape_guards += 1;
            for &off in &oplan.invariant_read_offsets {
                let vvar = builder.declare_var(types::I64);
                loop_licm_vars.insert((hdr, obj_r, off), vvar);
                hoisted_invariant_loads += 1;
            }
        }
        for (&arr_r, &kind) in &plan.hoisted_arrays {
            let pvar = builder.declare_var(ptr_ty);
            let lvar = builder.declare_var(types::I32);
            loop_arr_vars.insert((hdr, arr_r), (pvar, lvar, kind));
            hoisted_array_guards += 1;
        }
    }

    let sigs = OptTrampolineSigs::new(&mut builder, ptr_ty, call_conv);
    let len_slot =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 4, 2));

    // Prologue: load all live registers from `regs_ptr`, zero-init hoisted vars for dominance.
    builder.switch_to_block(prologue);
    let vm_ptr = builder.block_params(prologue)[0];
    let init_regs_ptr = builder.block_params(prologue)[1];
    let safepoint_mask = builder.block_params(prologue)[3];
    builder.def_var(regs_ptr_var, init_regs_ptr);
    let zero_i32 = builder.ins().iconst(types::I32, 0);
    let zero_ptr = builder.ins().iconst(ptr_ty, 0);
    let undef_init = builder.ins().iconst(types::I64, VAL_UNDEFINED);
    builder.def_var(loop_tick_var, zero_i32);

    for &v in loop_obj_ptr_vars.values() {
        builder.def_var(v, zero_ptr);
    }
    for &v in loop_licm_vars.values() {
        builder.def_var(v, undef_init);
    }
    for &(pv, lv, _) in loop_arr_vars.values() {
        builder.def_var(pv, zero_ptr);
        builder.def_var(lv, zero_i32);
    }
    for (i, &rv) in reg_vars.iter().enumerate() {
        let loaded = builder.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            init_regs_ptr,
            (i * 8) as i32,
        );
        builder.def_var(rv, loaded);
    }
    builder.ins().jump(pc_blocks[&0], &[]);

    let mut emitter = OptEmitter {
        builder: &mut builder,
        vm_ptr,
        safepoint_mask,
        regs_ptr_var,
        loop_tick_var,
        reg_vars: &reg_vars,
        pc_blocks: &pc_blocks,
        implicit_ret_block,
        exc_exit_block,
        sigs: &sigs,
        ptr_ty,
        len_slot,
        program_id,
        func_idx,
        has_closures,
        deopt_blocks: HashMap::new(),
        deopt_sites: Vec::new(),
        block_obj_slots: HashMap::new(),
        block_reg_aliases: HashMap::new(),
        inlined_call_sites: 0,
    };

    // Lower every bytecode instruction in linear PC order.
    let mut pc = 0usize;
    let mut block_terminated = true;
    while pc < func.code.len() {
        if let Some(&blk) = emitter.pc_blocks.get(&pc) {
            if !block_terminated {
                emitter.builder.ins().jump(blk, &[]);
            }
            emitter.builder.switch_to_block(blk);
            emitter.block_obj_slots.clear();
            emitter.block_reg_aliases.clear();
            block_terminated = false;

            // If this leader is a hoisted loop preheader, emit hoisted shape guards, LICM loads,
            // and packed-array guards ONCE here, then jump into `loop_body_blocks[&pc]`!
            if let Some(plan) = loop_plans.get(&pc) {
                let body_blk = loop_body_blocks[&pc];
                for (&obj_r, oplan) in &plan.hoisted_objects {
                    let obj_v = emitter.read_reg(obj_r);
                    let shape_v = emitter
                        .builder
                        .ins()
                        .iconst(types::I32, i64::from(oplan.shape_id));
                    let min_v = emitter
                        .builder
                        .ins()
                        .iconst(types::I32, i64::from(oplan.min_slots));
                    let fw_v = emitter
                        .builder
                        .ins()
                        .iconst(types::I32, i64::from(oplan.has_write));
                    let slots_ptr = emitter.call_trampoline_5(
                        rt_guard_object_slots as *const () as usize,
                        emitter.sigs.ptr_i64_u32_u32_u32_to_ptr,
                        &[emitter.vm_ptr, obj_v, shape_v, min_v, fw_v],
                    );
                    let is_null = emitter.builder.ins().icmp_imm_s(IntCC::Equal, slots_ptr, 0);
                    emitter.emit_deopt_if(is_null, pc as u32, DeoptReason::ShapeGuard);
                    emitter
                        .builder
                        .def_var(loop_obj_ptr_vars[&(pc, obj_r)], slots_ptr);

                    for &off in &oplan.invariant_read_offsets {
                        let loaded = emitter.builder.ins().load(
                            types::I64,
                            MemFlagsData::trusted(),
                            slots_ptr,
                            (off * 8) as i32,
                        );
                        emitter
                            .builder
                            .def_var(loop_licm_vars[&(pc, obj_r, off)], loaded);
                    }
                }
                for (&arr_r, &kind) in &plan.hoisted_arrays {
                    let arr_v = emitter.read_reg(arr_r);
                    let kind_code = match kind {
                        ElementsKind::PackedInt32 => 0i64,
                        ElementsKind::PackedFloat64 => 1i64,
                        ElementsKind::PackedValues => 2i64,
                        ElementsKind::SparseDict => 3i64,
                    };
                    let kind_v = emitter.builder.ins().iconst(types::I32, kind_code);
                    let len_ptr = emitter.builder.ins().stack_addr(ptr_ty, len_slot, 0);
                    let arr_ptr = emitter.call_trampoline_4(
                        rt_guard_packed_array as *const () as usize,
                        emitter.sigs.ptr_i64_u32_ptr_to_ptr,
                        &[emitter.vm_ptr, arr_v, kind_v, len_ptr],
                    );
                    let is_null = emitter.builder.ins().icmp_imm_s(IntCC::Equal, arr_ptr, 0);
                    emitter.emit_deopt_if(is_null, pc as u32, DeoptReason::ArrayGuard);
                    let arr_len =
                        emitter
                            .builder
                            .ins()
                            .load(types::I32, MemFlagsData::trusted(), len_ptr, 0);
                    let (pvar, lvar, _) = loop_arr_vars[&(pc, arr_r)];
                    emitter.builder.def_var(pvar, arr_ptr);
                    emitter.builder.def_var(lvar, arr_len);
                }

                emitter.builder.ins().jump(body_blk, &[]);
                emitter.builder.switch_to_block(body_blk);
                emitter.block_obj_slots.clear();
                emitter.block_reg_aliases.clear();
            }
        }
        if block_terminated {
            let op = func.code[pc].opcode().unwrap_or(Opcode::Nop);
            pc = advance_pc(func, pc, op);
            continue;
        }

        // Check if `pc` lies inside an active hoisted pure loop or any natural loop.
        let active_loop: Option<&LoopOptPlan> = loop_plans
            .values()
            .find(|p| pc >= p.header_pc && pc <= p.back_pc);
        let in_any_loop = all_loop_ranges.iter().any(|&(h, b)| pc >= h && pc <= b);

        let inst = func.code[pc];
        let op = inst.opcode().unwrap_or(Opcode::Nop);
        let next_pc = advance_pc(func, pc, op);
        let a = usize::from(inst.a());
        let b = usize::from(inst.b());
        let c = usize::from(inst.c());

        match op {
            Opcode::Nop | Opcode::ExtraArg | Opcode::Debugger => {}
            Opcode::CloseUpvalues => {
                if has_closures {
                    emitter.emit_generic_step(pc as u32);
                }
            }
            Opcode::Move => {
                let v = emitter.read_reg(b);
                emitter.record_move_alias(a, b);
                emitter.write_reg_preserving_alias(a, v);
            }
            Opcode::LoadUndef => {
                let v = emitter.builder.ins().iconst(types::I64, VAL_UNDEFINED);
                emitter.write_reg(a, v);
            }
            Opcode::LoadNull => {
                let v = emitter.builder.ins().iconst(types::I64, VAL_NULL);
                emitter.write_reg(a, v);
            }
            Opcode::LoadTrue => {
                let v = emitter.builder.ins().iconst(types::I64, VAL_TRUE);
                emitter.write_reg(a, v);
            }
            Opcode::LoadFalse => {
                let v = emitter.builder.ins().iconst(types::I64, VAL_FALSE);
                emitter.write_reg(a, v);
            }
            Opcode::LoadTdz => {
                let v = emitter
                    .builder
                    .ins()
                    .iconst(types::I64, JsValue::EMPTY.to_raw() as i64);
                emitter.write_reg(a, v);
            }
            Opcode::CheckTdz => {
                let v = emitter.read_reg(a);
                let is_empty = emitter.builder.ins().icmp_imm_s(
                    IntCC::Equal,
                    v,
                    JsValue::EMPTY.to_raw() as i64,
                );
                let err_blk = emitter.builder.create_block();
                let ok_blk = emitter.builder.create_block();
                emitter
                    .builder
                    .ins()
                    .brif(is_empty, err_blk, &[], ok_blk, &[]);
                emitter.builder.switch_to_block(err_blk);
                emitter.emit_generic_step(pc as u32);
                emitter.builder.ins().jump(ok_blk, &[]);
                emitter.builder.switch_to_block(ok_blk);
            }
            Opcode::LoadInt => {
                let raw = JsValue::from_i32(i32::from(inst.sbx())).to_raw() as i64;
                let v = emitter.builder.ins().iconst(types::I64, raw);
                emitter.write_reg(a, v);
            }
            Opcode::LoadConst => {
                let (idx, _) = resolve_wide(func, pc, true);
                let direct_raw = match func.constants.get(idx as usize) {
                    Some(ConstantEntry::Int32(iv)) => Some(JsValue::from_i32(*iv).to_raw()),
                    Some(ConstantEntry::Number(fv)) => Some(JsValue::from_number(*fv).to_raw()),
                    _ => None,
                };
                if let Some(raw) = direct_raw {
                    let v = emitter.builder.ins().iconst(types::I64, raw as i64);
                    emitter.write_reg(a, v);
                } else {
                    emitter.flush_all_regs();
                    let p_id = emitter
                        .builder
                        .ins()
                        .iconst(types::I32, i64::from(program_id));
                    let f_id = emitter
                        .builder
                        .ins()
                        .iconst(types::I32, i64::from(func_idx));
                    let c_id = emitter.builder.ins().iconst(types::I32, i64::from(idx));
                    let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                    let res = emitter.call_trampoline_5(
                        rt_load_const as *const () as usize,
                        emitter.sigs.ptr_u32_u32_u32_u32_to_i64,
                        &[emitter.vm_ptr, p_id, f_id, c_id, pc_v],
                    );
                    emitter.check_exception(res);
                    emitter.write_reg(a, res);
                }
            }
            Opcode::LoadThis => {
                let res = emitter.call_trampoline_1(
                    rt_load_this as *const () as usize,
                    emitter.sigs.ptr_to_i64,
                    emitter.vm_ptr,
                );
                emitter.write_reg(a, res);
            }
            Opcode::GetGlobal => {
                let (ic_idx, _) = resolve_wide(func, pc, true);
                let p_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(program_id));
                let f_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(func_idx));
                let ic_v = emitter.builder.ins().iconst(types::I32, i64::from(ic_idx));
                let fast_res = emitter.call_trampoline_4(
                    rt_get_global_ic_fast as *const () as usize,
                    emitter.sigs.ptr_u32_u32_u32_to_i64,
                    &[emitter.vm_ptr, p_id, f_id, ic_v],
                );
                let is_miss = emitter.builder.ins().icmp_imm_s(
                    IntCC::Equal,
                    fast_res,
                    JIT_MISS_SENTINEL as i64,
                );
                let slow_blk = emitter.builder.create_block();
                let merge_blk = emitter.builder.create_block();
                emitter.builder.append_block_param(merge_blk, types::I64);
                emitter
                    .builder
                    .ins()
                    .brif(is_miss, slow_blk, &[], merge_blk, &[fast_res.into()]);

                emitter.builder.switch_to_block(slow_blk);
                emitter.emit_generic_step(pc as u32);
                let reloaded = emitter.read_reg(a);
                emitter.builder.ins().jump(merge_blk, &[reloaded.into()]);

                emitter.builder.switch_to_block(merge_blk);
                let out = emitter.builder.block_params(merge_blk)[0];
                emitter.write_reg(a, out);
            }

            // --- Speculative Unboxed Int32 / Numeric Arithmetic ---
            Opcode::Add => emitter.emit_add_sub_mul(
                a,
                b,
                c,
                pc as u32,
                OptArith::Add,
                in_any_loop && hints.speculate_int32,
            ),
            Opcode::Sub => emitter.emit_add_sub_mul(
                a,
                b,
                c,
                pc as u32,
                OptArith::Sub,
                in_any_loop && hints.speculate_int32,
            ),
            Opcode::Mul => emitter.emit_add_sub_mul(
                a,
                b,
                c,
                pc as u32,
                OptArith::Mul,
                in_any_loop && hints.speculate_int32,
            ),
            Opcode::Div => emitter.emit_div_rem(a, b, c, pc as u32, false),
            Opcode::Rem => emitter.emit_div_rem(a, b, c, pc as u32, true),
            Opcode::Inc => {
                emitter.emit_inc_dec(a, b, pc as u32, true, in_any_loop && hints.speculate_int32)
            }
            Opcode::Dec => {
                emitter.emit_inc_dec(a, b, pc as u32, false, in_any_loop && hints.speculate_int32)
            }
            Opcode::Neg => emitter.emit_neg(a, b, pc as u32),

            // --- Bitwise ---
            Opcode::BitAnd
            | Opcode::BitOr
            | Opcode::BitXor
            | Opcode::Shl
            | Opcode::Shr
            | Opcode::UShr => emitter.emit_bitwise(op, a, b, c, pc as u32),
            Opcode::BitNot => emitter.emit_bitnot(a, b, pc as u32),

            // --- Relational & Equality ---
            Opcode::Lt => emitter.emit_rel_cmp(a, b, c, pc as u32, OptRel::Lt),
            Opcode::LtEq => emitter.emit_rel_cmp(a, b, c, pc as u32, OptRel::Le),
            Opcode::Gt => emitter.emit_rel_cmp(a, b, c, pc as u32, OptRel::Gt),
            Opcode::GtEq => emitter.emit_rel_cmp(a, b, c, pc as u32, OptRel::Ge),
            Opcode::StrictEq => emitter.emit_strict_eq(a, b, c, pc as u32, false),
            Opcode::StrictNotEq => emitter.emit_strict_eq(a, b, c, pc as u32, true),
            Opcode::Eq => emitter.emit_abstract_eq(a, b, c, pc as u32, false),
            Opcode::NotEq => emitter.emit_abstract_eq(a, b, c, pc as u32, true),
            Opcode::Not => emitter.emit_not(a, b),

            // --- Speculative Property Loads: LICM -> Hoisted Loop Slots -> Coalesced Block Slots -> Proto Guard -> IC ---
            Opcode::GetProp => {
                let (ic_idx, _) = resolve_wide(func, pc, false);
                let mut handled = false;
                let loop_obj_r = active_loop
                    .and_then(|p| p.pc_target_reg.get(&pc).copied())
                    .unwrap_or(b);

                // 1. Hoisted packed-array `.length` inside a pure loop!
                if let Some(plan) = active_loop {
                    if is_length_ic(func, ic_idx as usize) {
                        if let Some(&(_, lvar, _)) =
                            loop_arr_vars.get(&(plan.header_pc, loop_obj_r))
                        {
                            let len32 = emitter.builder.use_var(lvar);
                            let boxed = emitter.box_i32(len32);
                            emitter.write_reg(a, boxed);
                            handled = true;
                        }
                    }
                }

                // 2. Hoisted pure-loop LICM / direct `[slots_ptr + offset*8]` load!
                if !handled {
                    if let (Some(plan), Some(IcState::Monomorphic(entry))) =
                        (active_loop, ics.get(ic_idx as usize))
                    {
                        if entry.holder.is_none() && !entry.is_accessor {
                            if let Some(oplan) = plan.hoisted_objects.get(&loop_obj_r) {
                                if oplan.shape_id == entry.shape.0 {
                                    let val = if let Some(&lvar) = loop_licm_vars.get(&(
                                        plan.header_pc,
                                        loop_obj_r,
                                        entry.offset,
                                    )) {
                                        // Loop-invariant load hoisted completely out of the loop!
                                        emitter.builder.use_var(lvar)
                                    } else {
                                        let slots_ptr = emitter.builder.use_var(
                                            loop_obj_ptr_vars[&(plan.header_pc, loop_obj_r)],
                                        );
                                        emitter.builder.ins().load(
                                            types::I64,
                                            MemFlagsData::trusted(),
                                            slots_ptr,
                                            (entry.offset * 8) as i32,
                                        )
                                    };
                                    emitter.write_reg(a, val);
                                    handled = true;
                                }
                            }
                        }
                    }
                }

                // 3. Same-block coalesced shape guard (great for VDOM diff & object-heavy code!).
                if !handled && hints.speculate_shapes {
                    if let Some(IcState::Monomorphic(entry)) = ics.get(ic_idx as usize) {
                        if entry.holder.is_none() && !entry.is_accessor {
                            let canon_r = emitter.resolve_alias(b);
                            let (seg_min, seg_fw) = scan_block_object_requirements(
                                func,
                                &ics,
                                &leaders,
                                pc,
                                canon_r,
                                &emitter.block_reg_aliases,
                                entry.shape.0,
                                entry.offset + 1,
                                false,
                            );
                            let slots_ptr = emitter.guard_block_object_slots(
                                canon_r,
                                b,
                                entry.shape.0,
                                seg_min,
                                seg_fw,
                                pc as u32,
                            );
                            let val = emitter.builder.ins().load(
                                types::I64,
                                MemFlagsData::trusted(),
                                slots_ptr,
                                (entry.offset * 8) as i32,
                            );
                            emitter.write_reg(a, val);
                            handled = true;
                        } else if let Some(holder_h) = entry.holder {
                            if !entry.is_accessor && entry.proto_epoch == current_proto_epoch {
                                // 4. Speculative prototype-chain epoch + receiver prototype guard!
                                let obj_v = emitter.read_reg(b);
                                let packed_so =
                                    ((u64::from(entry.shape.0)) << 32) | u64::from(entry.offset);
                                let packed_v =
                                    emitter.builder.ins().iconst(types::I64, packed_so as i64);
                                let proto_raw =
                                    entry.receiver_proto.map_or(JsValue::NULL.to_raw(), |p| {
                                        JsValue::from_object(p).to_raw()
                                    });
                                let proto_v =
                                    emitter.builder.ins().iconst(types::I64, proto_raw as i64);
                                let holder_v = emitter.builder.ins().iconst(
                                    types::I64,
                                    JsValue::from_object(holder_h).to_raw() as i64,
                                );
                                let epoch_v = emitter
                                    .builder
                                    .ins()
                                    .iconst(types::I64, entry.proto_epoch as i64);
                                let res = emitter.call_trampoline_6(
                                    rt_guard_proto_prop as *const () as usize,
                                    emitter.sigs.ptr_i64_i64_i64_i64_i64_to_i64,
                                    &[emitter.vm_ptr, obj_v, packed_v, proto_v, holder_v, epoch_v],
                                );
                                let is_miss = emitter.builder.ins().icmp_imm_s(
                                    IntCC::Equal,
                                    res,
                                    JIT_MISS_SENTINEL as i64,
                                );
                                emitter.emit_deopt_if(
                                    is_miss,
                                    pc as u32,
                                    DeoptReason::PrototypeEpoch,
                                );
                                emitter.write_reg(a, res);
                                handled = true;
                            }
                        }
                    }
                }

                // 5. Fallback zero-flush Baseline IC path for polymorphic/megamorphic/accessor sites.
                if !handled {
                    emitter.emit_baseline_get_prop(a, b, ic_idx, pc as u32);
                }
            }

            // --- Speculative Property Stores: Hoisted Loop Slots -> Coalesced Block Slots -> IC ---
            Opcode::SetProp => {
                let (ic_idx, _) = resolve_wide(func, pc, false);
                let mut handled = false;
                let loop_obj_r = active_loop
                    .and_then(|p| p.pc_target_reg.get(&pc).copied())
                    .unwrap_or(a);

                if let (Some(plan), Some(IcState::Monomorphic(entry))) =
                    (active_loop, ics.get(ic_idx as usize))
                {
                    if entry.holder.is_none()
                        && !entry.is_accessor
                        && entry.target_shape == entry.shape
                    {
                        if let Some(oplan) = plan.hoisted_objects.get(&loop_obj_r) {
                            if oplan.shape_id == entry.shape.0 && oplan.has_write {
                                let slots_ptr = emitter
                                    .builder
                                    .use_var(loop_obj_ptr_vars[&(plan.header_pc, loop_obj_r)]);
                                let owner_v = emitter.read_reg(a);
                                let val_v = emitter.read_reg(b);
                                emitter.builder.ins().store(
                                    MemFlagsData::trusted(),
                                    val_v,
                                    slots_ptr,
                                    (entry.offset * 8) as i32,
                                );
                                emitter.emit_cold_write_barrier(owner_v, val_v);
                                handled = true;
                            }
                        }
                    }
                }

                if !handled && hints.speculate_shapes {
                    if let Some(IcState::Monomorphic(entry)) = ics.get(ic_idx as usize) {
                        if entry.holder.is_none()
                            && !entry.is_accessor
                            && entry.target_shape == entry.shape
                        {
                            let canon_r = emitter.resolve_alias(a);
                            let (seg_min, seg_fw) = scan_block_object_requirements(
                                func,
                                &ics,
                                &leaders,
                                pc,
                                canon_r,
                                &emitter.block_reg_aliases,
                                entry.shape.0,
                                entry.offset + 1,
                                true,
                            );
                            let slots_ptr = emitter.guard_block_object_slots(
                                canon_r,
                                a,
                                entry.shape.0,
                                seg_min,
                                seg_fw,
                                pc as u32,
                            );
                            let owner_v = emitter.read_reg(a);
                            let val_v = emitter.read_reg(b);
                            emitter.builder.ins().store(
                                MemFlagsData::trusted(),
                                val_v,
                                slots_ptr,
                                (entry.offset * 8) as i32,
                            );
                            emitter.emit_cold_write_barrier(owner_v, val_v);
                            handled = true;
                        }
                    }
                }

                if !handled {
                    emitter.emit_baseline_set_prop(a, b, ic_idx, pc as u32);
                }
            }

            // --- Speculative Hoisted Packed-Array Element Access ---
            Opcode::GetElem => {
                let mut handled = false;
                let loop_arr_r = active_loop
                    .and_then(|p| p.pc_target_reg.get(&pc).copied())
                    .unwrap_or(b);
                if let Some(plan) = active_loop {
                    if let Some(&(pvar, lvar, kind)) =
                        loop_arr_vars.get(&(plan.header_pc, loop_arr_r))
                    {
                        let idx_v = emitter.read_reg(c);
                        let is_i = emitter.is_int32(idx_v);
                        let not_i = emitter.builder.ins().icmp_imm_s(IntCC::Equal, is_i, 0);
                        emitter.emit_deopt_if(not_i, pc as u32, DeoptReason::Int32Type);
                        let idx32 = emitter.unbox_i32(idx_v);
                        let len32 = emitter.builder.use_var(lvar);
                        // Unsigned `idx32 >= len32` catches BOTH `idx < 0` and `idx >= len` in 1 instruction!
                        let oob = emitter.builder.ins().icmp(
                            IntCC::UnsignedGreaterThanOrEqual,
                            idx32,
                            len32,
                        );
                        emitter.emit_deopt_if(oob, pc as u32, DeoptReason::ArrayBounds);

                        let arr_ptr = emitter.builder.use_var(pvar);
                        let idx64 = emitter.builder.ins().uextend(ptr_ty, idx32);
                        let val = match kind {
                            ElementsKind::PackedInt32 => {
                                let byte_off = emitter.builder.ins().ishl_imm_s(idx64, 2);
                                let elem_ptr = emitter.builder.ins().iadd(arr_ptr, byte_off);
                                let raw32 = emitter.builder.ins().load(
                                    types::I32,
                                    MemFlagsData::trusted(),
                                    elem_ptr,
                                    0,
                                );
                                emitter.box_i32(raw32)
                            }
                            ElementsKind::PackedFloat64 => {
                                let byte_off = emitter.builder.ins().ishl_imm_s(idx64, 3);
                                let elem_ptr = emitter.builder.ins().iadd(arr_ptr, byte_off);
                                let f = emitter.builder.ins().load(
                                    types::F64,
                                    MemFlagsData::trusted(),
                                    elem_ptr,
                                    0,
                                );
                                emitter.box_f64(f)
                            }
                            ElementsKind::PackedValues => {
                                let byte_off = emitter.builder.ins().ishl_imm_s(idx64, 3);
                                let elem_ptr = emitter.builder.ins().iadd(arr_ptr, byte_off);
                                let raw64 = emitter.builder.ins().load(
                                    types::I64,
                                    MemFlagsData::trusted(),
                                    elem_ptr,
                                    0,
                                );
                                // Deopt if hole (`JsValue::EMPTY`).
                                let is_hole = emitter.builder.ins().icmp_imm_s(
                                    IntCC::Equal,
                                    raw64,
                                    JsValue::EMPTY.to_raw() as i64,
                                );
                                emitter.emit_deopt_if(is_hole, pc as u32, DeoptReason::ArrayGuard);
                                raw64
                            }
                            ElementsKind::SparseDict => unreachable!(),
                        };
                        emitter.write_reg(a, val);
                        handled = true;
                    }
                }
                if !handled {
                    emitter.emit_baseline_get_elem(a, b, c, pc as u32);
                }
            }
            Opcode::SetElem => {
                let mut handled = false;
                let loop_arr_r = active_loop
                    .and_then(|p| p.pc_target_reg.get(&pc).copied())
                    .unwrap_or(a);
                if let Some(plan) = active_loop {
                    if let Some(&(pvar, lvar, kind)) =
                        loop_arr_vars.get(&(plan.header_pc, loop_arr_r))
                    {
                        let idx_v = emitter.read_reg(b);
                        let val_v = emitter.read_reg(c);
                        let is_i = emitter.is_int32(idx_v);
                        let not_i = emitter.builder.ins().icmp_imm_s(IntCC::Equal, is_i, 0);
                        emitter.emit_deopt_if(not_i, pc as u32, DeoptReason::Int32Type);
                        let idx32 = emitter.unbox_i32(idx_v);
                        let len32 = emitter.builder.use_var(lvar);
                        let oob = emitter.builder.ins().icmp(
                            IntCC::UnsignedGreaterThanOrEqual,
                            idx32,
                            len32,
                        );
                        emitter.emit_deopt_if(oob, pc as u32, DeoptReason::ArrayBounds);

                        let arr_ptr = emitter.builder.use_var(pvar);
                        let idx64 = emitter.builder.ins().uextend(ptr_ty, idx32);
                        match kind {
                            ElementsKind::PackedInt32 => {
                                let v_is_i = emitter.is_int32(val_v);
                                let not_vi =
                                    emitter.builder.ins().icmp_imm_s(IntCC::Equal, v_is_i, 0);
                                emitter.emit_deopt_if(not_vi, pc as u32, DeoptReason::ArrayGuard);
                                let v32 = emitter.unbox_i32(val_v);
                                let byte_off = emitter.builder.ins().ishl_imm_s(idx64, 2);
                                let elem_ptr = emitter.builder.ins().iadd(arr_ptr, byte_off);
                                emitter.builder.ins().store(
                                    MemFlagsData::trusted(),
                                    v32,
                                    elem_ptr,
                                    0,
                                );
                            }
                            ElementsKind::PackedFloat64 => {
                                let v_is_f = emitter.is_float(val_v);
                                let v_is_i = emitter.is_int32(val_v);
                                let is_num = emitter.builder.ins().bor(v_is_f, v_is_i);
                                let not_num =
                                    emitter.builder.ins().icmp_imm_s(IntCC::Equal, is_num, 0);
                                emitter.emit_deopt_if(not_num, pc as u32, DeoptReason::ArrayGuard);
                                let vi = emitter.unbox_i32(val_v);
                                let vf_from_i =
                                    emitter.builder.ins().fcvt_from_sint(types::F64, vi);
                                let vf_raw = emitter.unbox_f64(val_v);
                                let vf = emitter.builder.ins().select(v_is_i, vf_from_i, vf_raw);
                                let byte_off = emitter.builder.ins().ishl_imm_s(idx64, 3);
                                let elem_ptr = emitter.builder.ins().iadd(arr_ptr, byte_off);
                                emitter.builder.ins().store(
                                    MemFlagsData::trusted(),
                                    vf,
                                    elem_ptr,
                                    0,
                                );
                            }
                            ElementsKind::PackedValues => {
                                let byte_off = emitter.builder.ins().ishl_imm_s(idx64, 3);
                                let elem_ptr = emitter.builder.ins().iadd(arr_ptr, byte_off);
                                emitter.builder.ins().store(
                                    MemFlagsData::trusted(),
                                    val_v,
                                    elem_ptr,
                                    0,
                                );
                                let owner_v = emitter.read_reg(a);
                                emitter.emit_cold_write_barrier(owner_v, val_v);
                            }
                            ElementsKind::SparseDict => unreachable!(),
                        }
                        handled = true;
                    }
                }
                if !handled {
                    emitter.emit_baseline_set_elem(a, b, c, pc as u32);
                }
            }

            // --- Speculative Small Pure Leaf Callee Inlining & Fast Direct Call ---
            Opcode::Call => {
                if let Some(cand) = inline_map.get(&pc) {
                    emitter.emit_inlined_call(a, b, c, pc as u32, cand);
                } else if c <= 2 && func_idx < 0x00FF_FFFF {
                    emitter.block_obj_slots.clear();
                    emitter.flush_all_regs();
                    let callee_v = emitter.read_reg(b);
                    let undef = emitter.builder.ins().iconst(types::I64, VAL_UNDEFINED);
                    let a0 = if c >= 1 {
                        emitter.read_reg(b + 1)
                    } else {
                        undef
                    };
                    let a1 = if c >= 2 {
                        emitter.read_reg(b + 2)
                    } else {
                        undef
                    };
                    let packed = (u64::from(program_id) << 32)
                        | ((u64::from(func_idx) & 0x00FF_FFFF) << 8)
                        | (c as u64);
                    let meta_v = emitter.builder.ins().iconst(types::I64, packed as i64);
                    let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                    let res = emitter.call_trampoline_6(
                        rt_call_fast2 as *const () as usize,
                        emitter.sigs.ptr_i64_i64_i64_i64_u32_to_i64,
                        &[emitter.vm_ptr, callee_v, a0, a1, meta_v, pc_v],
                    );
                    emitter.check_exception(res);
                    emitter.reload_regs_ptr_and_maybe_all();
                    emitter.write_reg(a, res);
                } else {
                    emitter.emit_generic_step(pc as u32);
                }
            }

            // --- Control Flow & Loop Back-Edge ---
            Opcode::Jump => {
                let target = ((pc + 1) as isize + isize::from(inst.sbx())) as usize;
                let blk = emitter.target_block(target);
                emitter.builder.ins().jump(blk, &[]);
                block_terminated = true;
            }
            Opcode::JumpIfFalse => {
                let target = ((pc + 1) as isize + isize::from(inst.sbx())) as usize;
                let v = emitter.read_reg(a);
                let cond = emitter.emit_is_truthy(v);
                let taken = emitter.target_block(target);
                let fall = emitter.target_block(next_pc);
                emitter.builder.ins().brif(cond, fall, &[], taken, &[]);
                block_terminated = true;
            }
            Opcode::JumpIfTrue => {
                let target = ((pc + 1) as isize + isize::from(inst.sbx())) as usize;
                let v = emitter.read_reg(a);
                let cond = emitter.emit_is_truthy(v);
                let taken = emitter.target_block(target);
                let fall = emitter.target_block(next_pc);
                emitter.builder.ins().brif(cond, taken, &[], fall, &[]);
                block_terminated = true;
            }
            Opcode::JumpIfNullish => {
                let target = ((pc + 1) as isize + isize::from(inst.sbx())) as usize;
                let v = emitter.read_reg(a);
                let is_u = emitter
                    .builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, v, VAL_UNDEFINED);
                let is_n = emitter.builder.ins().icmp_imm_s(IntCC::Equal, v, VAL_NULL);
                let is_nullish = emitter.builder.ins().bor(is_u, is_n);
                let taken = emitter.target_block(target);
                let fall = emitter.target_block(next_pc);
                emitter
                    .builder
                    .ins()
                    .brif(is_nullish, taken, &[], fall, &[]);
                block_terminated = true;
            }
            Opcode::JumpIfNotUndef => {
                let target = ((pc + 1) as isize + isize::from(inst.sbx())) as usize;
                let v = emitter.read_reg(a);
                let not_u = emitter
                    .builder
                    .ins()
                    .icmp_imm_s(IntCC::NotEqual, v, VAL_UNDEFINED);
                let taken = emitter.target_block(target);
                let fall = emitter.target_block(next_pc);
                emitter.builder.ins().brif(not_u, taken, &[], fall, &[]);
                block_terminated = true;
            }
            Opcode::LoopBack => {
                let target = ((pc + 1) as isize + isize::from(inst.sbx())) as usize;
                // Jump straight to `loop_body_blocks[&target]` (skipping preheader guards!) when hoisted!
                let header = loop_body_blocks
                    .get(&target)
                    .copied()
                    .unwrap_or_else(|| emitter.target_block(target));
                let sp_blk = emitter.builder.create_block();

                let tick = emitter.builder.use_var(emitter.loop_tick_var);
                let next_tick = emitter.builder.ins().iadd_imm_s(tick, 1);
                emitter.builder.def_var(emitter.loop_tick_var, next_tick);
                let masked = emitter
                    .builder
                    .ins()
                    .band(next_tick, emitter.safepoint_mask);
                let do_sp = emitter.builder.ins().icmp_imm_s(IntCC::Equal, masked, 0);
                emitter.builder.ins().brif(do_sp, sp_blk, &[], header, &[]);

                emitter.builder.switch_to_block(sp_blk);
                emitter.flush_all_regs();
                let p_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(program_id));
                let f_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(func_idx));
                let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                let res = emitter.call_trampoline_4(
                    rt_loop_safepoint as *const () as usize,
                    emitter.sigs.ptr_u32_u32_u32_to_i64,
                    &[emitter.vm_ptr, p_id, f_id, pc_v],
                );
                emitter.check_exception(res);
                emitter.builder.ins().jump(header, &[]);
                block_terminated = true;
            }
            Opcode::Return => {
                let ret_val = emitter.read_reg(a);
                if has_closures {
                    emitter.flush_all_regs();
                }
                emitter.builder.ins().return_(&[ret_val]);
                block_terminated = true;
            }
            Opcode::Throw => {
                let val = emitter.read_reg(a);
                emitter.flush_all_regs();
                let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                let _ = emitter.call_trampoline_3(
                    rt_throw as *const () as usize,
                    emitter.sigs.ptr_i64_u32_to_i64,
                    &[emitter.vm_ptr, val, pc_v],
                );
                emitter.builder.ins().jump(emitter.exc_exit_block, &[]);
                block_terminated = true;
            }
            _ => {
                emitter.emit_generic_step(pc as u32);
            }
        }

        pc = next_pc;
    }

    if !block_terminated {
        emitter.builder.ins().jump(implicit_ret_block, &[]);
    }

    // Emit every lazily-created Deopt stub block: refresh `regs_ptr`, flush ALL live SSA
    // registers directly into `vm.registers[base..]`, and return `reason.encode_deopt(resume_pc)`.
    let deopt_entries: Vec<((u32, u16), Block)> =
        emitter.deopt_blocks.iter().map(|(&k, &b)| (k, b)).collect();
    for ((resume_pc, reason_code), dblk) in deopt_entries {
        emitter.builder.switch_to_block(dblk);
        let fresh_regs = emitter.call_trampoline_1(
            rt_regs_ptr as *const () as usize,
            emitter.sigs.ptr_to_ptr,
            emitter.vm_ptr,
        );
        emitter.builder.def_var(emitter.regs_ptr_var, fresh_regs);
        emitter.flush_all_regs();
        let encoded = crate::trampolines::JIT_DEOPT_TAG
            | (u64::from(reason_code) << 32)
            | u64::from(resume_pc);
        let ret = emitter.builder.ins().iconst(types::I64, encoded as i64);
        emitter.builder.ins().return_(&[ret]);
    }

    // Implicit `return undefined` epilogue.
    emitter.builder.switch_to_block(implicit_ret_block);
    if has_closures {
        emitter.flush_all_regs();
    }
    let undef = emitter.builder.ins().iconst(types::I64, VAL_UNDEFINED);
    emitter.builder.ins().return_(&[undef]);

    // Exception unwind exit.
    emitter.builder.switch_to_block(exc_exit_block);
    let fresh_regs = emitter.call_trampoline_1(
        rt_regs_ptr as *const () as usize,
        emitter.sigs.ptr_to_ptr,
        emitter.vm_ptr,
    );
    emitter.builder.def_var(emitter.regs_ptr_var, fresh_regs);
    emitter.flush_all_regs();
    let sentinel = emitter
        .builder
        .ins()
        .iconst(types::I64, JIT_EXCEPTION_SENTINEL as i64);
    emitter.builder.ins().return_(&[sentinel]);

    let deopt_sites = emitter.deopt_sites;
    let inlined_call_sites = emitter.inlined_call_sites;

    builder.seal_all_blocks();
    builder.finalize(isa.frontend_config());

    let mut ctx = Context::for_function(clif_func);
    let mut ctrl_plane = ControlPlane::default();
    let compiled = ctx
        .compile(&*isa, &mut ctrl_plane)
        .map_err(|e| JitCompileError::Cranelift(format!("{e:?}")))?;

    let code_page = CodePage::from_machine_code(compiled.code_buffer())?;
    Ok(Arc::new(CompiledOptimized {
        code_page,
        reg_count,
        deopt_sites,
        hoisted_shape_guards,
        hoisted_invariant_loads,
        hoisted_array_guards,
        inlined_call_sites,
    }))
}

#[derive(Clone, Copy)]
enum OptArith {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Copy)]
enum OptRel {
    Lt,
    Le,
    Gt,
    Ge,
}

struct OptTrampolineSigs {
    ptr_to_ptr: cranelift_codegen::ir::SigRef,
    ptr_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_i64_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_u32_i64_i64_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_u32_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_u32_u32_u32_to_ptr: cranelift_codegen::ir::SigRef,
    ptr_i64_u32_ptr_to_ptr: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_i64_i64_i64_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_u32_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_i64_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_i64_i64_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_u32_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
}

impl OptTrampolineSigs {
    fn new(builder: &mut FunctionBuilder<'_>, ptr: types::Type, cc: CallConv) -> Self {
        let mk = |b: &mut FunctionBuilder<'_>, params: &[types::Type], ret: types::Type| {
            let mut s = Signature::new(cc);
            for &p in params {
                s.params.push(AbiParam::new(p));
            }
            s.returns.push(AbiParam::new(ret));
            b.import_signature(s)
        };
        Self {
            ptr_to_ptr: mk(builder, &[ptr], ptr),
            ptr_to_i64: mk(builder, &[ptr], types::I64),
            ptr_i64_to_i64: mk(builder, &[ptr, types::I64], types::I64),
            ptr_i64_i64_to_i64: mk(builder, &[ptr, types::I64, types::I64], types::I64),
            ptr_i64_i64_i64_to_i64: mk(
                builder,
                &[ptr, types::I64, types::I64, types::I64],
                types::I64,
            ),
            ptr_i64_u32_to_i64: mk(builder, &[ptr, types::I64, types::I32], types::I64),
            ptr_i64_i64_u32_to_i64: mk(
                builder,
                &[ptr, types::I64, types::I64, types::I32],
                types::I64,
            ),
            ptr_u32_i64_i64_u32_to_i64: mk(
                builder,
                &[ptr, types::I32, types::I64, types::I64, types::I32],
                types::I64,
            ),
            ptr_u32_u32_u32_to_i64: mk(
                builder,
                &[ptr, types::I32, types::I32, types::I32],
                types::I64,
            ),
            ptr_u32_u32_u32_u32_to_i64: mk(
                builder,
                &[ptr, types::I32, types::I32, types::I32, types::I32],
                types::I64,
            ),
            ptr_i64_u32_u32_u32_to_i64: mk(
                builder,
                &[ptr, types::I64, types::I32, types::I32, types::I32],
                types::I64,
            ),
            ptr_i64_u32_u32_u32_to_ptr: mk(
                builder,
                &[ptr, types::I64, types::I32, types::I32, types::I32],
                ptr,
            ),
            ptr_i64_u32_ptr_to_ptr: mk(builder, &[ptr, types::I64, types::I32, ptr], ptr),
            ptr_i64_i64_i64_i64_i64_to_i64: mk(
                builder,
                &[
                    ptr,
                    types::I64,
                    types::I64,
                    types::I64,
                    types::I64,
                    types::I64,
                ],
                types::I64,
            ),
            ptr_i64_u32_u32_u32_u32_to_i64: mk(
                builder,
                &[
                    ptr,
                    types::I64,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
            ptr_i64_i64_i64_u32_to_i64: mk(
                builder,
                &[ptr, types::I64, types::I64, types::I64, types::I32],
                types::I64,
            ),
            ptr_i64_i64_i64_i64_u32_to_i64: mk(
                builder,
                &[
                    ptr,
                    types::I64,
                    types::I64,
                    types::I64,
                    types::I64,
                    types::I32,
                ],
                types::I64,
            ),
            ptr_i64_i64_u32_u32_u32_to_i64: mk(
                builder,
                &[
                    ptr,
                    types::I64,
                    types::I64,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
            ptr_i64_i64_u32_u32_u32_u32_to_i64: mk(
                builder,
                &[
                    ptr,
                    types::I64,
                    types::I64,
                    types::I32,
                    types::I32,
                    types::I32,
                    types::I32,
                ],
                types::I64,
            ),
        }
    }
}

struct OptEmitter<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    vm_ptr: Value,
    safepoint_mask: Value,
    regs_ptr_var: Variable,
    loop_tick_var: Variable,
    reg_vars: &'a [Variable],
    pc_blocks: &'a HashMap<usize, Block>,
    implicit_ret_block: Block,
    exc_exit_block: Block,
    sigs: &'a OptTrampolineSigs,
    ptr_ty: types::Type,
    #[allow(dead_code)]
    len_slot: StackSlot,
    program_id: u32,
    func_idx: u32,
    has_closures: bool,
    deopt_blocks: HashMap<(u32, u16), Block>,
    deopt_sites: Vec<DeoptMetadata>,
    /// Per-basic-block coalesced shape guard cache: `(canon_obj_reg, shape_id) -> (slots_ptr, min_slots, for_write)`.
    block_obj_slots: HashMap<(usize, u32), (Value, u32, bool)>,
    /// Intra-block `Move dst, src` alias tracking mapping temps back to canonical object registers.
    block_reg_aliases: HashMap<usize, usize>,
    inlined_call_sites: usize,
}

impl OptEmitter<'_, '_> {
    #[inline]
    fn resolve_alias(&self, r: usize) -> usize {
        self.block_reg_aliases.get(&r).copied().unwrap_or(r)
    }

    #[inline]
    fn record_move_alias(&mut self, dst: usize, src: usize) {
        let canon = self.resolve_alias(src);
        self.block_obj_slots.retain(|&(reg, _), _| reg != dst);
        self.block_reg_aliases.retain(|_, &mut v| v != dst);
        if canon != dst {
            self.block_reg_aliases.insert(dst, canon);
        } else {
            self.block_reg_aliases.remove(&dst);
        }
    }

    #[inline]
    fn read_reg(&mut self, r: usize) -> Value {
        if let Some(&v) = self.reg_vars.get(r) {
            self.builder.use_var(v)
        } else {
            self.builder.ins().iconst(types::I64, VAL_UNDEFINED)
        }
    }

    #[inline]
    fn write_reg_preserving_alias(&mut self, r: usize, val: Value) {
        if let Some(&v) = self.reg_vars.get(r) {
            self.builder.def_var(v, val);
        }
    }

    #[inline]
    fn write_reg(&mut self, r: usize, val: Value) {
        self.block_obj_slots.retain(|&(reg, _), _| reg != r);
        self.block_reg_aliases.remove(&r);
        self.block_reg_aliases.retain(|_, &mut v| v != r);
        if let Some(&v) = self.reg_vars.get(r) {
            self.builder.def_var(v, val);
        }
    }

    fn flush_all_regs(&mut self) {
        let ptr = self.builder.use_var(self.regs_ptr_var);
        for (i, &rv) in self.reg_vars.iter().enumerate() {
            let val = self.builder.use_var(rv);
            self.builder
                .ins()
                .store(MemFlagsData::trusted(), val, ptr, (i * 8) as i32);
        }
    }

    fn reload_all_regs(&mut self) {
        let new_ptr = self.call_trampoline_1(
            rt_regs_ptr as *const () as usize,
            self.sigs.ptr_to_ptr,
            self.vm_ptr,
        );
        self.builder.def_var(self.regs_ptr_var, new_ptr);
        for (i, &rv) in self.reg_vars.iter().enumerate() {
            let val = self.builder.ins().load(
                types::I64,
                MemFlagsData::trusted(),
                new_ptr,
                (i * 8) as i32,
            );
            self.builder.def_var(rv, val);
        }
    }

    fn reload_regs_ptr_and_maybe_all(&mut self) {
        let new_ptr = self.call_trampoline_1(
            rt_regs_ptr as *const () as usize,
            self.sigs.ptr_to_ptr,
            self.vm_ptr,
        );
        self.builder.def_var(self.regs_ptr_var, new_ptr);
        if self.has_closures {
            for (i, &rv) in self.reg_vars.iter().enumerate() {
                let val = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::trusted(),
                    new_ptr,
                    (i * 8) as i32,
                );
                self.builder.def_var(rv, val);
            }
        }
    }

    fn target_block(&self, pc: usize) -> Block {
        self.pc_blocks
            .get(&pc)
            .copied()
            .unwrap_or(self.implicit_ret_block)
    }

    fn get_or_create_deopt_block(&mut self, resume_pc: u32, reason: DeoptReason) -> Block {
        let key = (resume_pc, reason as u16);
        if let Some(&b) = self.deopt_blocks.get(&key) {
            return b;
        }
        let b = self.builder.create_block();
        self.deopt_blocks.insert(key, b);
        self.deopt_sites.push(DeoptMetadata { resume_pc, reason });
        b
    }

    /// Emits a cold deoptimization branch when `cond != 0`.
    fn emit_deopt_if(&mut self, cond: Value, resume_pc: u32, reason: DeoptReason) {
        let dblk = self.get_or_create_deopt_block(resume_pc, reason);
        let cont = self.builder.create_block();
        self.builder.ins().brif(cond, dblk, &[], cont, &[]);
        self.builder.switch_to_block(cont);
    }

    /// Coalesced per-basic-block shape guard: reuses a previously validated `slots_ptr`
    /// in the same block when `min_slots` and `for_write` are already satisfied.
    fn guard_block_object_slots(
        &mut self,
        canon_r: usize,
        read_r: usize,
        shape_id: u32,
        min_slots: u32,
        for_write: bool,
        pc: u32,
    ) -> Value {
        if let Some(&(ptr, prev_min, prev_fw)) = self.block_obj_slots.get(&(canon_r, shape_id)) {
            if prev_min >= min_slots && (!for_write || prev_fw) {
                return ptr;
            }
        }
        let obj_v = self.read_reg(read_r);
        let shape_v = self.builder.ins().iconst(types::I32, i64::from(shape_id));
        let min_v = self.builder.ins().iconst(types::I32, i64::from(min_slots));
        let fw_v = self.builder.ins().iconst(types::I32, i64::from(for_write));
        let slots_ptr = self.call_trampoline_5(
            rt_guard_object_slots as *const () as usize,
            self.sigs.ptr_i64_u32_u32_u32_to_ptr,
            &[self.vm_ptr, obj_v, shape_v, min_v, fw_v],
        );
        let is_null = self.builder.ins().icmp_imm_s(IntCC::Equal, slots_ptr, 0);
        self.emit_deopt_if(is_null, pc, DeoptReason::ShapeGuard);
        self.block_obj_slots
            .insert((canon_r, shape_id), (slots_ptr, min_slots, for_write));
        slots_ptr
    }

    /// Cold generational GC write barrier emitted after direct raw-slot stores.
    /// Zero-cost branch not-taken for numbers (`f64`/`Int32`), booleans, `null`, `undefined`.
    fn emit_cold_write_barrier(&mut self, owner_v: Value, val_v: Value) {
        let is_heap =
            self.builder
                .ins()
                .icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, val_v, TAG_STRING_MIN);
        let wb_blk = self.builder.create_block();
        let cont_blk = self.builder.create_block();
        self.builder.ins().brif(is_heap, wb_blk, &[], cont_blk, &[]);
        self.builder.switch_to_block(wb_blk);
        let _ = self.call_trampoline_3(
            rt_write_barrier as *const () as usize,
            self.sigs.ptr_i64_i64_to_i64,
            &[self.vm_ptr, owner_v, val_v],
        );
        self.builder.ins().jump(cont_blk, &[]);
        self.builder.switch_to_block(cont_blk);
    }

    fn call_trampoline_1(
        &mut self,
        fn_addr: usize,
        sig: cranelift_codegen::ir::SigRef,
        a0: Value,
    ) -> Value {
        let callee = self.builder.ins().iconst(self.ptr_ty, fn_addr as i64);
        let call = self.builder.ins().call_indirect(sig, callee, &[a0]);
        self.builder.inst_results(call)[0]
    }

    fn call_trampoline_2(
        &mut self,
        fn_addr: usize,
        sig: cranelift_codegen::ir::SigRef,
        args: &[Value; 2],
    ) -> Value {
        let callee = self.builder.ins().iconst(self.ptr_ty, fn_addr as i64);
        let call = self.builder.ins().call_indirect(sig, callee, args);
        self.builder.inst_results(call)[0]
    }

    fn call_trampoline_3(
        &mut self,
        fn_addr: usize,
        sig: cranelift_codegen::ir::SigRef,
        args: &[Value; 3],
    ) -> Value {
        let callee = self.builder.ins().iconst(self.ptr_ty, fn_addr as i64);
        let call = self.builder.ins().call_indirect(sig, callee, args);
        self.builder.inst_results(call)[0]
    }

    fn call_trampoline_4(
        &mut self,
        fn_addr: usize,
        sig: cranelift_codegen::ir::SigRef,
        args: &[Value; 4],
    ) -> Value {
        let callee = self.builder.ins().iconst(self.ptr_ty, fn_addr as i64);
        let call = self.builder.ins().call_indirect(sig, callee, args);
        self.builder.inst_results(call)[0]
    }

    fn call_trampoline_5(
        &mut self,
        fn_addr: usize,
        sig: cranelift_codegen::ir::SigRef,
        args: &[Value; 5],
    ) -> Value {
        let callee = self.builder.ins().iconst(self.ptr_ty, fn_addr as i64);
        let call = self.builder.ins().call_indirect(sig, callee, args);
        self.builder.inst_results(call)[0]
    }

    fn call_trampoline_6(
        &mut self,
        fn_addr: usize,
        sig: cranelift_codegen::ir::SigRef,
        args: &[Value; 6],
    ) -> Value {
        let callee = self.builder.ins().iconst(self.ptr_ty, fn_addr as i64);
        let call = self.builder.ins().call_indirect(sig, callee, args);
        self.builder.inst_results(call)[0]
    }

    fn call_trampoline_7(
        &mut self,
        fn_addr: usize,
        sig: cranelift_codegen::ir::SigRef,
        args: &[Value; 7],
    ) -> Value {
        let callee = self.builder.ins().iconst(self.ptr_ty, fn_addr as i64);
        let call = self.builder.ins().call_indirect(sig, callee, args);
        self.builder.inst_results(call)[0]
    }

    fn check_exception(&mut self, val: Value) {
        let is_exc =
            self.builder
                .ins()
                .icmp_imm_s(IntCC::Equal, val, JIT_EXCEPTION_SENTINEL as i64);
        let ok_blk = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_exc, self.exc_exit_block, &[], ok_blk, &[]);
        self.builder.switch_to_block(ok_blk);
    }

    fn emit_generic_step(&mut self, pc: u32) {
        self.block_obj_slots.clear();
        self.block_reg_aliases.clear();
        self.flush_all_regs();
        let p_id = self
            .builder
            .ins()
            .iconst(types::I32, i64::from(self.program_id));
        let f_id = self
            .builder
            .ins()
            .iconst(types::I32, i64::from(self.func_idx));
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let res = self.call_trampoline_4(
            rt_step_generic as *const () as usize,
            self.sigs.ptr_u32_u32_u32_to_i64,
            &[self.vm_ptr, p_id, f_id, pc_v],
        );
        self.check_exception(res);
        self.reload_all_regs();
    }

    /// Inlines a verified pure acyclic leaf callee directly into the caller's Cranelift IR!
    /// Guards exact callee identity first; any guard failure inside the pure callee cleanly
    /// deoptimizes back to `call_pc` in the caller with zero side effects committed.
    fn emit_inlined_call(
        &mut self,
        dst: usize,
        call_base: usize,
        argc: usize,
        call_pc: u32,
        cand: &InlineCandidate,
    ) {
        self.inlined_call_sites += 1;

        // Guard callee object identity (`callee_reg == expected_callee_raw`, else deopt to `call_pc`).
        let callee_val = self.read_reg(call_base);
        let mismatch =
            self.builder
                .ins()
                .icmp_imm_s(IntCC::NotEqual, callee_val, cand.callee_raw as i64);
        self.emit_deopt_if(mismatch, call_pc, DeoptReason::CalleeGuard);

        let callee_reg_count =
            (cand.func.register_count as usize).max(cand.func.param_count as usize + 1);
        let callee_vars: Vec<Variable> = (0..callee_reg_count)
            .map(|_| self.builder.declare_var(types::I64))
            .collect();
        let undef = self.builder.ins().iconst(types::I64, VAL_UNDEFINED);
        for (i, &cv) in callee_vars.iter().enumerate() {
            let init = if i < argc {
                self.read_reg(call_base + 1 + i)
            } else {
                undef
            };
            self.builder.def_var(cv, init);
        }

        // Pre-create blocks for callee leaders and a shared return join block.
        let mut leaders = BTreeSet::new();
        leaders.insert(0usize);
        let mut spc = 0usize;
        while spc < cand.func.code.len() {
            let inst = cand.func.code[spc];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(&cand.func, spc, op);
            if matches!(
                op,
                Opcode::Jump
                    | Opcode::JumpIfFalse
                    | Opcode::JumpIfTrue
                    | Opcode::JumpIfNullish
                    | Opcode::JumpIfNotUndef
            ) {
                let target = ((spc + 1) as isize + isize::from(inst.sbx())) as usize;
                leaders.insert(target);
                leaders.insert(next_pc);
            } else if op == Opcode::Return {
                leaders.insert(next_pc);
            }
            spc = next_pc;
        }

        let ret_join = self.builder.create_block();
        self.builder.append_block_param(ret_join, types::I64);
        let mut cblocks: HashMap<usize, Block> = HashMap::new();
        for &l in &leaders {
            if l < cand.func.code.len() {
                cblocks.insert(l, self.builder.create_block());
            }
        }
        self.builder.ins().jump(cblocks[&0], &[]);

        let mut cpc = 0usize;
        let mut term = true;
        while cpc < cand.func.code.len() {
            if let Some(&blk) = cblocks.get(&cpc) {
                if !term {
                    self.builder.ins().jump(blk, &[]);
                }
                self.builder.switch_to_block(blk);
                term = false;
            }
            if term {
                let op = cand.func.code[cpc].opcode().unwrap_or(Opcode::Nop);
                cpc = advance_pc(&cand.func, cpc, op);
                continue;
            }

            let inst = cand.func.code[cpc];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(&cand.func, cpc, op);
            let ca = usize::from(inst.a());
            let cb = usize::from(inst.b());
            let cc = usize::from(inst.c());
            let target_blk = |t: usize| cblocks.get(&t).copied().unwrap_or(ret_join);

            match op {
                Opcode::Nop | Opcode::ExtraArg => {}
                Opcode::Move => {
                    let v = self.builder.use_var(callee_vars[cb]);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::LoadUndef => {
                    let v = self.builder.ins().iconst(types::I64, VAL_UNDEFINED);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::LoadNull => {
                    let v = self.builder.ins().iconst(types::I64, VAL_NULL);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::LoadTrue => {
                    let v = self.builder.ins().iconst(types::I64, VAL_TRUE);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::LoadFalse => {
                    let v = self.builder.ins().iconst(types::I64, VAL_FALSE);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::LoadTdz => {
                    let v = self
                        .builder
                        .ins()
                        .iconst(types::I64, JsValue::EMPTY.to_raw() as i64);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::CheckTdz => {
                    let v = self.builder.use_var(callee_vars[ca]);
                    let is_empty = self.builder.ins().icmp_imm_s(
                        IntCC::Equal,
                        v,
                        JsValue::EMPTY.to_raw() as i64,
                    );
                    self.emit_deopt_if(is_empty, call_pc, DeoptReason::CalleeGuard);
                }
                Opcode::LoadInt => {
                    let raw = JsValue::from_i32(i32::from(inst.sbx())).to_raw() as i64;
                    let v = self.builder.ins().iconst(types::I64, raw);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::LoadConst => {
                    let (idx, _) = resolve_wide(&cand.func, cpc, true);
                    let raw = match cand.func.constants.get(idx as usize) {
                        Some(ConstantEntry::Int32(iv)) => JsValue::from_i32(*iv).to_raw(),
                        Some(ConstantEntry::Number(fv)) => JsValue::from_number(*fv).to_raw(),
                        _ => JsValue::UNDEFINED.to_raw(),
                    };
                    let v = self.builder.ins().iconst(types::I64, raw as i64);
                    self.builder.def_var(callee_vars[ca], v);
                }
                Opcode::Add | Opcode::Sub | Opcode::Mul => {
                    let lhs = self.builder.use_var(callee_vars[cb]);
                    let rhs = self.builder.use_var(callee_vars[cc]);
                    let aop = match op {
                        Opcode::Add => OptArith::Add,
                        Opcode::Sub => OptArith::Sub,
                        _ => OptArith::Mul,
                    };
                    let res = self.emit_speculative_arith_vals(lhs, rhs, call_pc, aop);
                    self.builder.def_var(callee_vars[ca], res);
                }
                Opcode::Div | Opcode::Rem => {
                    let lhs = self.builder.use_var(callee_vars[cb]);
                    let rhs = self.builder.use_var(callee_vars[cc]);
                    let l_i = self.is_int32(lhs);
                    let r_i = self.is_int32(rhs);
                    let l_f = self.is_float(lhs);
                    let r_f = self.is_float(rhs);
                    let l_num = self.builder.ins().bor(l_i, l_f);
                    let r_num = self.builder.ins().bor(r_i, r_f);
                    let both_num = self.builder.ins().band(l_num, r_num);
                    let not_num = self.builder.ins().icmp_imm_s(IntCC::Equal, both_num, 0);
                    self.emit_deopt_if(not_num, call_pc, DeoptReason::Int32Type);
                    if op == Opcode::Div {
                        let li = self.unbox_i32(lhs);
                        let ri = self.unbox_i32(rhs);
                        let li_f = self.builder.ins().fcvt_from_sint(types::F64, li);
                        let lf_raw = self.unbox_f64(lhs);
                        let lf = self.builder.ins().select(l_i, li_f, lf_raw);
                        let ri_f = self.builder.ins().fcvt_from_sint(types::F64, ri);
                        let rf_raw = self.unbox_f64(rhs);
                        let rf = self.builder.ins().select(r_i, ri_f, rf_raw);
                        let q = self.builder.ins().fdiv(lf, rf);
                        let bq = self.box_f64(q);
                        self.builder.def_var(callee_vars[ca], bq);
                    } else {
                        let both_i = self.builder.ins().band(l_i, r_i);
                        let not_i = self.builder.ins().icmp_imm_s(IntCC::Equal, both_i, 0);
                        self.emit_deopt_if(not_i, call_pc, DeoptReason::Int32Type);
                        let li = self.unbox_i32(lhs);
                        let ri = self.unbox_i32(rhs);
                        let l_nn =
                            self.builder
                                .ins()
                                .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, li, 0);
                        let r_p = self
                            .builder
                            .ins()
                            .icmp_imm_s(IntCC::SignedGreaterThan, ri, 0);
                        let ok = self.builder.ins().band(l_nn, r_p);
                        let bad = self.builder.ins().icmp_imm_s(IntCC::Equal, ok, 0);
                        self.emit_deopt_if(bad, call_pc, DeoptReason::Int32Overflow);
                        let rem = self.builder.ins().srem(li, ri);
                        let br = self.box_i32(rem);
                        self.builder.def_var(callee_vars[ca], br);
                    }
                }
                Opcode::Inc | Opcode::Dec => {
                    let src = self.builder.use_var(callee_vars[cb]);
                    let one = self
                        .builder
                        .ins()
                        .iconst(types::I64, JsValue::from_i32(1).to_raw() as i64);
                    let aop = if op == Opcode::Inc {
                        OptArith::Add
                    } else {
                        OptArith::Sub
                    };
                    let res = self.emit_speculative_arith_vals(src, one, call_pc, aop);
                    self.builder.def_var(callee_vars[ca], res);
                }
                Opcode::Neg => {
                    let src = self.builder.use_var(callee_vars[cb]);
                    let is_i = self.is_int32(src);
                    let not_i = self.builder.ins().icmp_imm_s(IntCC::Equal, is_i, 0);
                    self.emit_deopt_if(not_i, call_pc, DeoptReason::Int32Type);
                    let si = self.unbox_i32(src);
                    let is_zero = self.builder.ins().icmp_imm_s(IntCC::Equal, si, 0);
                    let is_min =
                        self.builder
                            .ins()
                            .icmp_imm_s(IntCC::Equal, si, i64::from(i32::MIN));
                    let ovf = self.builder.ins().bor(is_zero, is_min);
                    self.emit_deopt_if(ovf, call_pc, DeoptReason::Int32Overflow);
                    let neg = self.builder.ins().ineg(si);
                    let bn = self.box_i32(neg);
                    self.builder.def_var(callee_vars[ca], bn);
                }
                Opcode::BitAnd
                | Opcode::BitOr
                | Opcode::BitXor
                | Opcode::Shl
                | Opcode::Shr
                | Opcode::UShr => {
                    let lhs = self.builder.use_var(callee_vars[cb]);
                    let rhs = self.builder.use_var(callee_vars[cc]);
                    let both_i = {
                        let li = self.is_int32(lhs);
                        let ri = self.is_int32(rhs);
                        self.builder.ins().band(li, ri)
                    };
                    let not_i = self.builder.ins().icmp_imm_s(IntCC::Equal, both_i, 0);
                    self.emit_deopt_if(not_i, call_pc, DeoptReason::Int32Type);
                    let li = self.unbox_i32(lhs);
                    let ri = self.unbox_i32(rhs);
                    let res = match op {
                        Opcode::BitAnd => {
                            let r = self.builder.ins().band(li, ri);
                            self.box_i32(r)
                        }
                        Opcode::BitOr => {
                            let r = self.builder.ins().bor(li, ri);
                            self.box_i32(r)
                        }
                        Opcode::BitXor => {
                            let r = self.builder.ins().bxor(li, ri);
                            self.box_i32(r)
                        }
                        Opcode::Shl => {
                            let sh = self.builder.ins().band_imm_s(ri, 0x1F);
                            let r = self.builder.ins().ishl(li, sh);
                            self.box_i32(r)
                        }
                        Opcode::Shr => {
                            let sh = self.builder.ins().band_imm_s(ri, 0x1F);
                            let r = self.builder.ins().sshr(li, sh);
                            self.box_i32(r)
                        }
                        Opcode::UShr => {
                            let sh = self.builder.ins().band_imm_s(ri, 0x1F);
                            let r = self.builder.ins().ushr(li, sh);
                            let nonneg = self.builder.ins().icmp_imm_s(
                                IntCC::SignedGreaterThanOrEqual,
                                r,
                                0,
                            );
                            let neg = self.builder.ins().icmp_imm_s(IntCC::Equal, nonneg, 0);
                            self.emit_deopt_if(neg, call_pc, DeoptReason::Int32Overflow);
                            self.box_i32(r)
                        }
                        _ => unreachable!(),
                    };
                    self.builder.def_var(callee_vars[ca], res);
                }
                Opcode::BitNot => {
                    let src = self.builder.use_var(callee_vars[cb]);
                    let is_i = self.is_int32(src);
                    let not_i = self.builder.ins().icmp_imm_s(IntCC::Equal, is_i, 0);
                    self.emit_deopt_if(not_i, call_pc, DeoptReason::Int32Type);
                    let si = self.unbox_i32(src);
                    let inv = self.builder.ins().bnot(si);
                    let b_inv = self.box_i32(inv);
                    self.builder.def_var(callee_vars[ca], b_inv);
                }
                Opcode::Lt | Opcode::LtEq | Opcode::Gt | Opcode::GtEq => {
                    let lhs = self.builder.use_var(callee_vars[cb]);
                    let rhs = self.builder.use_var(callee_vars[cc]);
                    let li_chk = self.is_int32(lhs);
                    let ri_chk = self.is_int32(rhs);
                    let both_i = self.builder.ins().band(li_chk, ri_chk);
                    let not_i = self.builder.ins().icmp_imm_s(IntCC::Equal, both_i, 0);
                    self.emit_deopt_if(not_i, call_pc, DeoptReason::Int32Type);
                    let li = self.unbox_i32(lhs);
                    let ri = self.unbox_i32(rhs);
                    let icc = match op {
                        Opcode::Lt => IntCC::SignedLessThan,
                        Opcode::LtEq => IntCC::SignedLessThanOrEqual,
                        Opcode::Gt => IntCC::SignedGreaterThan,
                        _ => IntCC::SignedGreaterThanOrEqual,
                    };
                    let cond = self.builder.ins().icmp(icc, li, ri);
                    let bval = self.box_bool(cond);
                    self.builder.def_var(callee_vars[ca], bval);
                }
                Opcode::StrictEq | Opcode::StrictNotEq => {
                    let lhs = self.builder.use_var(callee_vars[cb]);
                    let rhs = self.builder.use_var(callee_vars[cc]);
                    // Fast exact bit comparison for non-float values (Int32, bool, null, undef, obj, symbol);
                    // deopt on floats/strings so NaN / -0.0 / rope-string semantics stay 100% exact.
                    let l_is_i = self.is_int32(lhs);
                    let r_is_i = self.is_int32(rhs);
                    let both_i = self.builder.ins().band(l_is_i, r_is_i);
                    let not_i = self.builder.ins().icmp_imm_s(IntCC::Equal, both_i, 0);
                    self.emit_deopt_if(not_i, call_pc, DeoptReason::Int32Type);
                    let cc_op = if op == Opcode::StrictNotEq {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    };
                    let cond = self.builder.ins().icmp(cc_op, lhs, rhs);
                    let bval = self.box_bool(cond);
                    self.builder.def_var(callee_vars[ca], bval);
                }
                Opcode::Not => {
                    let v = self.builder.use_var(callee_vars[cb]);
                    let tr = self.emit_is_truthy(v);
                    let fl = self.builder.ins().icmp_imm_s(IntCC::Equal, tr, 0);
                    let bval = self.box_bool(fl);
                    self.builder.def_var(callee_vars[ca], bval);
                }
                Opcode::GetProp => {
                    let (ic_idx, _) = resolve_wide(&cand.func, cpc, false);
                    let Some(IcState::Monomorphic(entry)) = cand.ic_snapshot.get(ic_idx as usize)
                    else {
                        unreachable!();
                    };
                    let obj_v = self.builder.use_var(callee_vars[cb]);
                    let shape_v = self
                        .builder
                        .ins()
                        .iconst(types::I32, i64::from(entry.shape.0));
                    let min_v = self
                        .builder
                        .ins()
                        .iconst(types::I32, i64::from(entry.offset + 1));
                    let fw_v = self.builder.ins().iconst(types::I32, 0);
                    let slots_ptr = self.call_trampoline_5(
                        rt_guard_object_slots as *const () as usize,
                        self.sigs.ptr_i64_u32_u32_u32_to_ptr,
                        &[self.vm_ptr, obj_v, shape_v, min_v, fw_v],
                    );
                    let is_null = self.builder.ins().icmp_imm_s(IntCC::Equal, slots_ptr, 0);
                    self.emit_deopt_if(is_null, call_pc, DeoptReason::ShapeGuard);
                    let loaded = self.builder.ins().load(
                        types::I64,
                        MemFlagsData::trusted(),
                        slots_ptr,
                        (entry.offset * 8) as i32,
                    );
                    self.builder.def_var(callee_vars[ca], loaded);
                }
                Opcode::Jump => {
                    let target = ((cpc + 1) as isize + isize::from(inst.sbx())) as usize;
                    let tb = target_blk(target);
                    if tb == ret_join {
                        let u = self.builder.ins().iconst(types::I64, VAL_UNDEFINED);
                        self.builder.ins().jump(ret_join, &[u.into()]);
                    } else {
                        self.builder.ins().jump(tb, &[]);
                    }
                    term = true;
                }
                Opcode::JumpIfFalse | Opcode::JumpIfTrue => {
                    let target = ((cpc + 1) as isize + isize::from(inst.sbx())) as usize;
                    let v = self.builder.use_var(callee_vars[ca]);
                    let cond = self.emit_is_truthy(v);
                    let taken = target_blk(target);
                    let fall = target_blk(next_pc);
                    let u = self.builder.ins().iconst(types::I64, VAL_UNDEFINED);
                    let taken_args: &[cranelift_codegen::ir::BlockArg] =
                        if taken == ret_join { &[u.into()] } else { &[] };
                    let fall_args: &[cranelift_codegen::ir::BlockArg] =
                        if fall == ret_join { &[u.into()] } else { &[] };
                    if op == Opcode::JumpIfFalse {
                        self.builder
                            .ins()
                            .brif(cond, fall, fall_args, taken, taken_args);
                    } else {
                        self.builder
                            .ins()
                            .brif(cond, taken, taken_args, fall, fall_args);
                    }
                    term = true;
                }
                Opcode::JumpIfNullish => {
                    let target = ((cpc + 1) as isize + isize::from(inst.sbx())) as usize;
                    let v = self.builder.use_var(callee_vars[ca]);
                    let is_u = self
                        .builder
                        .ins()
                        .icmp_imm_s(IntCC::Equal, v, VAL_UNDEFINED);
                    let is_n = self.builder.ins().icmp_imm_s(IntCC::Equal, v, VAL_NULL);
                    let is_nullish = self.builder.ins().bor(is_u, is_n);
                    let taken = target_blk(target);
                    let fall = target_blk(next_pc);
                    let u = self.builder.ins().iconst(types::I64, VAL_UNDEFINED);
                    let ta: &[cranelift_codegen::ir::BlockArg] =
                        if taken == ret_join { &[u.into()] } else { &[] };
                    let fa: &[cranelift_codegen::ir::BlockArg] =
                        if fall == ret_join { &[u.into()] } else { &[] };
                    self.builder.ins().brif(is_nullish, taken, ta, fall, fa);
                    term = true;
                }
                Opcode::JumpIfNotUndef => {
                    let target = ((cpc + 1) as isize + isize::from(inst.sbx())) as usize;
                    let v = self.builder.use_var(callee_vars[ca]);
                    let not_u = self
                        .builder
                        .ins()
                        .icmp_imm_s(IntCC::NotEqual, v, VAL_UNDEFINED);
                    let taken = target_blk(target);
                    let fall = target_blk(next_pc);
                    let u = self.builder.ins().iconst(types::I64, VAL_UNDEFINED);
                    let ta: &[cranelift_codegen::ir::BlockArg] =
                        if taken == ret_join { &[u.into()] } else { &[] };
                    let fa: &[cranelift_codegen::ir::BlockArg] =
                        if fall == ret_join { &[u.into()] } else { &[] };
                    self.builder.ins().brif(not_u, taken, ta, fall, fa);
                    term = true;
                }
                Opcode::Return => {
                    let rv = self.builder.use_var(callee_vars[ca]);
                    self.builder.ins().jump(ret_join, &[rv.into()]);
                    term = true;
                }
                _ => unreachable!("check_inline_candidate filtered opcodes"),
            }
            cpc = next_pc;
        }
        if !term {
            let u = self.builder.ins().iconst(types::I64, VAL_UNDEFINED);
            self.builder.ins().jump(ret_join, &[u.into()]);
        }

        self.builder.switch_to_block(ret_join);
        let result = self.builder.block_params(ret_join)[0];
        self.write_reg(dst, result);
    }

    // --- NaN-Boxing Helpers ---

    #[inline]
    fn is_int32(&mut self, v: Value) -> Value {
        let tag = self.builder.ins().band_imm_s(v, TAG_MASK);
        self.builder.ins().icmp_imm_s(IntCC::Equal, tag, TAG_INT32)
    }

    #[inline]
    fn is_float(&mut self, v: Value) -> Value {
        self.builder
            .ins()
            .icmp_imm_s(IntCC::UnsignedLessThan, v, TAG_THRESHOLD)
    }

    #[inline]
    fn unbox_i32(&mut self, v: Value) -> Value {
        self.builder.ins().ireduce(types::I32, v)
    }

    #[inline]
    fn box_i32(&mut self, i: Value) -> Value {
        let ext = self.builder.ins().uextend(types::I64, i);
        self.builder.ins().bor_imm_s(ext, TAG_INT32)
    }

    #[inline]
    fn box_f64(&mut self, f: Value) -> Value {
        let is_nan = self.builder.ins().fcmp(FloatCC::Unordered, f, f);
        let bits = self
            .builder
            .ins()
            .bitcast(types::I64, MemFlagsData::new(), f);
        let canon = self.builder.ins().iconst(types::I64, CANONICAL_NAN);
        self.builder.ins().select(is_nan, canon, bits)
    }

    #[inline]
    fn unbox_f64(&mut self, v: Value) -> Value {
        self.builder
            .ins()
            .bitcast(types::F64, MemFlagsData::new(), v)
    }

    #[inline]
    fn box_bool(&mut self, cond: Value) -> Value {
        let t = self.builder.ins().iconst(types::I64, VAL_TRUE);
        let f = self.builder.ins().iconst(types::I64, VAL_FALSE);
        self.builder.ins().select(cond, t, f)
    }

    /// Straight-line speculative unboxed `Int32` arithmetic with exact overflow & type deopt.
    fn emit_speculative_arith_vals(
        &mut self,
        lhs: Value,
        rhs: Value,
        pc: u32,
        op: OptArith,
    ) -> Value {
        let l_i = self.is_int32(lhs);
        let r_i = self.is_int32(rhs);
        let both_i = self.builder.ins().band(l_i, r_i);
        let not_i = self.builder.ins().icmp_imm_s(IntCC::Equal, both_i, 0);
        self.emit_deopt_if(not_i, pc, DeoptReason::Int32Type);

        let li = self.unbox_i32(lhs);
        let ri = self.unbox_i32(rhs);
        let l64 = self.builder.ins().sextend(types::I64, li);
        let r64 = self.builder.ins().sextend(types::I64, ri);
        let res64 = match op {
            OptArith::Add => self.builder.ins().iadd(l64, r64),
            OptArith::Sub => self.builder.ins().isub(l64, r64),
            OptArith::Mul => self.builder.ins().imul(l64, r64),
        };
        let res32 = self.builder.ins().ireduce(types::I32, res64);
        let back64 = self.builder.ins().sextend(types::I64, res32);
        let overflowed = self.builder.ins().icmp(IntCC::NotEqual, res64, back64);

        let must_deopt = if matches!(op, OptArith::Mul) {
            let is_zero = self.builder.ins().icmp_imm_s(IntCC::Equal, res32, 0);
            let xor_sign = self.builder.ins().bxor(li, ri);
            let neg_sign = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::SignedLessThan, xor_sign, 0);
            let neg_zero = self.builder.ins().band(is_zero, neg_sign);
            self.builder.ins().bor(overflowed, neg_zero)
        } else {
            overflowed
        };
        self.emit_deopt_if(must_deopt, pc, DeoptReason::Int32Overflow);
        self.box_i32(res32)
    }

    fn emit_add_sub_mul(
        &mut self,
        dst: usize,
        lhs_r: usize,
        rhs_r: usize,
        pc: u32,
        op: OptArith,
        speculate_i32: bool,
    ) {
        let lhs = self.read_reg(lhs_r);
        let rhs = self.read_reg(rhs_r);
        if speculate_i32 {
            let out = self.emit_speculative_arith_vals(lhs, rhs, pc, op);
            self.write_reg(dst, out);
            return;
        }

        // Non-speculative full NaN-boxed fast path (handles Int32 + f64 + overflow + slow path).
        let l_i32 = self.is_int32(lhs);
        let r_i32 = self.is_int32(rhs);
        let both_i32 = self.builder.ins().band(l_i32, r_i32);

        let fast_i32 = self.builder.create_block();
        let check_num = self.builder.create_block();
        let fast_f64 = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(both_i32, fast_i32, &[], check_num, &[]);

        self.builder.switch_to_block(fast_i32);
        let li = self.unbox_i32(lhs);
        let ri = self.unbox_i32(rhs);
        let l64 = self.builder.ins().sextend(types::I64, li);
        let r64 = self.builder.ins().sextend(types::I64, ri);
        let res64 = match op {
            OptArith::Add => self.builder.ins().iadd(l64, r64),
            OptArith::Sub => self.builder.ins().isub(l64, r64),
            OptArith::Mul => self.builder.ins().imul(l64, r64),
        };
        let res32 = self.builder.ins().ireduce(types::I32, res64);
        let back64 = self.builder.ins().sextend(types::I64, res32);
        let fits = self.builder.ins().icmp(IntCC::Equal, res64, back64);
        let use_i32 = if matches!(op, OptArith::Mul) {
            let is_zero = self.builder.ins().icmp_imm_s(IntCC::Equal, res32, 0);
            let xor_sign = self.builder.ins().bxor(li, ri);
            let neg_sign = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::SignedLessThan, xor_sign, 0);
            let neg_zero = self.builder.ins().band(is_zero, neg_sign);
            let not_nz = self.builder.ins().icmp_imm_s(IntCC::Equal, neg_zero, 0);
            self.builder.ins().band(fits, not_nz)
        } else {
            fits
        };
        let i_ok = self.builder.create_block();
        let i_ovf = self.builder.create_block();
        self.builder.ins().brif(use_i32, i_ok, &[], i_ovf, &[]);

        self.builder.switch_to_block(i_ok);
        let bi = self.box_i32(res32);
        self.builder.ins().jump(merge_blk, &[bi.into()]);

        self.builder.switch_to_block(i_ovf);
        let lf = self.builder.ins().fcvt_from_sint(types::F64, li);
        let rf = self.builder.ins().fcvt_from_sint(types::F64, ri);
        let fr = match op {
            OptArith::Add => self.builder.ins().fadd(lf, rf),
            OptArith::Sub => self.builder.ins().fsub(lf, rf),
            OptArith::Mul => self.builder.ins().fmul(lf, rf),
        };
        let bo = self.box_f64(fr);
        self.builder.ins().jump(merge_blk, &[bo.into()]);

        self.builder.switch_to_block(check_num);
        let l_f = self.is_float(lhs);
        let r_f = self.is_float(rhs);
        let l_num = self.builder.ins().bor(l_i32, l_f);
        let r_num = self.builder.ins().bor(r_i32, r_f);
        let both_num = self.builder.ins().band(l_num, r_num);
        self.builder
            .ins()
            .brif(both_num, fast_f64, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_f64);
        let li_u = self.unbox_i32(lhs);
        let ri_u = self.unbox_i32(rhs);
        let lf_raw = self.unbox_f64(lhs);
        let lf_from_i = self.builder.ins().fcvt_from_sint(types::F64, li_u);
        let lf_v = self.builder.ins().select(l_i32, lf_from_i, lf_raw);
        let rf_raw = self.unbox_f64(rhs);
        let rf_from_i = self.builder.ins().fcvt_from_sint(types::F64, ri_u);
        let rf_v = self.builder.ins().select(r_i32, rf_from_i, rf_raw);
        let fres = match op {
            OptArith::Add => self.builder.ins().fadd(lf_v, rf_v),
            OptArith::Sub => self.builder.ins().fsub(lf_v, rf_v),
            OptArith::Mul => self.builder.ins().fmul(lf_v, rf_v),
        };
        let bf = self.box_f64(fres);
        self.builder.ins().jump(merge_blk, &[bf.into()]);

        self.builder.switch_to_block(slow_blk);
        self.flush_all_regs();
        let fn_addr = match op {
            OptArith::Add => rt_add_slow as *const () as usize,
            OptArith::Sub => rt_sub_slow as *const () as usize,
            OptArith::Mul => rt_mul_slow as *const () as usize,
        };
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let slow_res = self.call_trampoline_4(
            fn_addr,
            self.sigs.ptr_i64_i64_u32_to_i64,
            &[self.vm_ptr, lhs, rhs, pc_v],
        );
        self.check_exception(slow_res);
        self.reload_regs_ptr_and_maybe_all();
        self.builder.ins().jump(merge_blk, &[slow_res.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_div_rem(&mut self, dst: usize, lhs_r: usize, rhs_r: usize, pc: u32, is_rem: bool) {
        let lhs = self.read_reg(lhs_r);
        let rhs = self.read_reg(rhs_r);
        let l_i = self.is_int32(lhs);
        let r_i = self.is_int32(rhs);

        if !is_rem {
            let l_f = self.is_float(lhs);
            let r_f = self.is_float(rhs);
            let l_num = self.builder.ins().bor(l_i, l_f);
            let r_num = self.builder.ins().bor(r_i, r_f);
            let both_num = self.builder.ins().band(l_num, r_num);

            let fast_blk = self.builder.create_block();
            let slow_blk = self.builder.create_block();
            let merge_blk = self.builder.create_block();
            self.builder.append_block_param(merge_blk, types::I64);
            self.builder
                .ins()
                .brif(both_num, fast_blk, &[], slow_blk, &[]);

            self.builder.switch_to_block(fast_blk);
            let li = self.unbox_i32(lhs);
            let ri = self.unbox_i32(rhs);
            let li_f = self.builder.ins().fcvt_from_sint(types::F64, li);
            let lf_raw = self.unbox_f64(lhs);
            let lf = self.builder.ins().select(l_i, li_f, lf_raw);
            let ri_f = self.builder.ins().fcvt_from_sint(types::F64, ri);
            let rf_raw = self.unbox_f64(rhs);
            let rf = self.builder.ins().select(r_i, ri_f, rf_raw);
            let q = self.builder.ins().fdiv(lf, rf);
            let bq = self.box_f64(q);
            self.builder.ins().jump(merge_blk, &[bq.into()]);

            self.builder.switch_to_block(slow_blk);
            self.flush_all_regs();
            let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
            let res = self.call_trampoline_4(
                rt_div_slow as *const () as usize,
                self.sigs.ptr_i64_i64_u32_to_i64,
                &[self.vm_ptr, lhs, rhs, pc_v],
            );
            self.check_exception(res);
            self.reload_regs_ptr_and_maybe_all();
            self.builder.ins().jump(merge_blk, &[res.into()]);

            self.builder.switch_to_block(merge_blk);
            let out = self.builder.block_params(merge_blk)[0];
            self.write_reg(dst, out);
        } else {
            let both_i = self.builder.ins().band(l_i, r_i);
            let fast_i = self.builder.create_block();
            let slow_blk = self.builder.create_block();
            let merge_blk = self.builder.create_block();
            self.builder.append_block_param(merge_blk, types::I64);
            self.builder.ins().brif(both_i, fast_i, &[], slow_blk, &[]);

            self.builder.switch_to_block(fast_i);
            let li = self.unbox_i32(lhs);
            let ri = self.unbox_i32(rhs);
            let l_nn = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, li, 0);
            let r_p = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::SignedGreaterThan, ri, 0);
            let safe = self.builder.ins().band(l_nn, r_p);
            let do_rem = self.builder.create_block();
            self.builder.ins().brif(safe, do_rem, &[], slow_blk, &[]);

            self.builder.switch_to_block(do_rem);
            let rem = self.builder.ins().srem(li, ri);
            let br = self.box_i32(rem);
            self.builder.ins().jump(merge_blk, &[br.into()]);

            self.builder.switch_to_block(slow_blk);
            self.flush_all_regs();
            let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
            let res = self.call_trampoline_4(
                rt_rem_slow as *const () as usize,
                self.sigs.ptr_i64_i64_u32_to_i64,
                &[self.vm_ptr, lhs, rhs, pc_v],
            );
            self.check_exception(res);
            self.reload_regs_ptr_and_maybe_all();
            self.builder.ins().jump(merge_blk, &[res.into()]);

            self.builder.switch_to_block(merge_blk);
            let out = self.builder.block_params(merge_blk)[0];
            self.write_reg(dst, out);
        }
    }

    fn emit_inc_dec(
        &mut self,
        dst: usize,
        src_r: usize,
        pc: u32,
        is_inc: bool,
        speculate_i32: bool,
    ) {
        let src = self.read_reg(src_r);
        if speculate_i32 {
            let one = self
                .builder
                .ins()
                .iconst(types::I64, JsValue::from_i32(1).to_raw() as i64);
            let out = self.emit_speculative_arith_vals(
                src,
                one,
                pc,
                if is_inc { OptArith::Add } else { OptArith::Sub },
            );
            self.write_reg(dst, out);
            return;
        }
        let is_i = self.is_int32(src);
        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder.ins().brif(is_i, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let si = self.unbox_i32(src);
        let limit = if is_inc { i32::MAX } else { i32::MIN };
        let ok = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::NotEqual, si, i64::from(limit));
        let ok_blk = self.builder.create_block();
        self.builder.ins().brif(ok, ok_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(ok_blk);
        let next = self
            .builder
            .ins()
            .iadd_imm_s(si, if is_inc { 1 } else { -1 });
        let bn = self.box_i32(next);
        self.builder.ins().jump(merge_blk, &[bn.into()]);

        self.builder.switch_to_block(slow_blk);
        self.emit_generic_step(pc);
        let reloaded = self.read_reg(dst);
        self.builder.ins().jump(merge_blk, &[reloaded.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_neg(&mut self, dst: usize, src_r: usize, pc: u32) {
        let src = self.read_reg(src_r);
        let is_i = self.is_int32(src);
        let fast_i = self.builder.create_block();
        let check_f = self.builder.create_block();
        let fast_f = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder.ins().brif(is_i, fast_i, &[], check_f, &[]);

        self.builder.switch_to_block(fast_i);
        let si = self.unbox_i32(src);
        let not_zero = self.builder.ins().icmp_imm_s(IntCC::NotEqual, si, 0);
        let not_min = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::NotEqual, si, i64::from(i32::MIN));
        let ok = self.builder.ins().band(not_zero, not_min);
        let ok_blk = self.builder.create_block();
        self.builder.ins().brif(ok, ok_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(ok_blk);
        let neg = self.builder.ins().ineg(si);
        let bn = self.box_i32(neg);
        self.builder.ins().jump(merge_blk, &[bn.into()]);

        self.builder.switch_to_block(check_f);
        let is_f = self.is_float(src);
        self.builder.ins().brif(is_f, fast_f, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_f);
        let sf = self.unbox_f64(src);
        let nf = self.builder.ins().fneg(sf);
        let bnf = self.box_f64(nf);
        self.builder.ins().jump(merge_blk, &[bnf.into()]);

        self.builder.switch_to_block(slow_blk);
        self.emit_generic_step(pc);
        let rel = self.read_reg(dst);
        self.builder.ins().jump(merge_blk, &[rel.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_bitwise(&mut self, op: Opcode, dst: usize, lhs_r: usize, rhs_r: usize, pc: u32) {
        let lhs = self.read_reg(lhs_r);
        let rhs = self.read_reg(rhs_r);
        let l_i = self.is_int32(lhs);
        let r_i = self.is_int32(rhs);
        let both_i = self.builder.ins().band(l_i, r_i);

        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(both_i, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let li = self.unbox_i32(lhs);
        let ri = self.unbox_i32(rhs);
        let res = match op {
            Opcode::BitAnd => {
                let r = self.builder.ins().band(li, ri);
                self.box_i32(r)
            }
            Opcode::BitOr => {
                let r = self.builder.ins().bor(li, ri);
                self.box_i32(r)
            }
            Opcode::BitXor => {
                let r = self.builder.ins().bxor(li, ri);
                self.box_i32(r)
            }
            Opcode::Shl => {
                let sh = self.builder.ins().band_imm_s(ri, 0x1F);
                let r = self.builder.ins().ishl(li, sh);
                self.box_i32(r)
            }
            Opcode::Shr => {
                let sh = self.builder.ins().band_imm_s(ri, 0x1F);
                let r = self.builder.ins().sshr(li, sh);
                self.box_i32(r)
            }
            Opcode::UShr => {
                let sh = self.builder.ins().band_imm_s(ri, 0x1F);
                let r = self.builder.ins().ushr(li, sh);
                let fits = self
                    .builder
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, r, 0);
                let r_i32 = self.box_i32(r);
                let r_u64 = self.builder.ins().uextend(types::I64, r);
                let r_f = self.builder.ins().fcvt_from_uint(types::F64, r_u64);
                let r_f64 = self.box_f64(r_f);
                self.builder.ins().select(fits, r_i32, r_f64)
            }
            _ => unreachable!(),
        };
        self.builder.ins().jump(merge_blk, &[res.into()]);

        self.builder.switch_to_block(slow_blk);
        self.emit_generic_step(pc);
        let rel = self.read_reg(dst);
        self.builder.ins().jump(merge_blk, &[rel.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_bitnot(&mut self, dst: usize, src_r: usize, pc: u32) {
        let src = self.read_reg(src_r);
        let is_i = self.is_int32(src);
        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder.ins().brif(is_i, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let si = self.unbox_i32(src);
        let inv = self.builder.ins().bnot(si);
        let bi = self.box_i32(inv);
        self.builder.ins().jump(merge_blk, &[bi.into()]);

        self.builder.switch_to_block(slow_blk);
        self.emit_generic_step(pc);
        let rel = self.read_reg(dst);
        self.builder.ins().jump(merge_blk, &[rel.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_rel_cmp(&mut self, dst: usize, lhs_r: usize, rhs_r: usize, pc: u32, op: OptRel) {
        let lhs = self.read_reg(lhs_r);
        let rhs = self.read_reg(rhs_r);
        let l_i = self.is_int32(lhs);
        let r_i = self.is_int32(rhs);
        let both_i = self.builder.ins().band(l_i, r_i);

        let fast_i = self.builder.create_block();
        let check_f = self.builder.create_block();
        let fast_f = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder.ins().brif(both_i, fast_i, &[], check_f, &[]);

        self.builder.switch_to_block(fast_i);
        let li = self.unbox_i32(lhs);
        let ri = self.unbox_i32(rhs);
        let icc = match op {
            OptRel::Lt => IntCC::SignedLessThan,
            OptRel::Le => IntCC::SignedLessThanOrEqual,
            OptRel::Gt => IntCC::SignedGreaterThan,
            OptRel::Ge => IntCC::SignedGreaterThanOrEqual,
        };
        let cond_i = self.builder.ins().icmp(icc, li, ri);
        let bi = self.box_bool(cond_i);
        self.builder.ins().jump(merge_blk, &[bi.into()]);

        self.builder.switch_to_block(check_f);
        let l_f = self.is_float(lhs);
        let r_f = self.is_float(rhs);
        let l_num = self.builder.ins().bor(l_i, l_f);
        let r_num = self.builder.ins().bor(r_i, r_f);
        let both_num = self.builder.ins().band(l_num, r_num);
        self.builder
            .ins()
            .brif(both_num, fast_f, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_f);
        let li_u = self.unbox_i32(lhs);
        let ri_u = self.unbox_i32(rhs);
        let li_f = self.builder.ins().fcvt_from_sint(types::F64, li_u);
        let lf_raw = self.unbox_f64(lhs);
        let lf = self.builder.ins().select(l_i, li_f, lf_raw);
        let ri_f = self.builder.ins().fcvt_from_sint(types::F64, ri_u);
        let rf_raw = self.unbox_f64(rhs);
        let rf = self.builder.ins().select(r_i, ri_f, rf_raw);
        let fcc = match op {
            OptRel::Lt => FloatCC::LessThan,
            OptRel::Le => FloatCC::LessThanOrEqual,
            OptRel::Gt => FloatCC::GreaterThan,
            OptRel::Ge => FloatCC::GreaterThanOrEqual,
        };
        let cond_f = self.builder.ins().fcmp(fcc, lf, rf);
        let bf = self.box_bool(cond_f);
        self.builder.ins().jump(merge_blk, &[bf.into()]);

        self.builder.switch_to_block(slow_blk);
        self.flush_all_regs();
        let op_code = match op {
            OptRel::Lt => 0,
            OptRel::Le => 1,
            OptRel::Gt => 2,
            OptRel::Ge => 3,
        };
        let op_v = self.builder.ins().iconst(types::I32, op_code);
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let res = self.call_trampoline_5(
            rt_cmp_slow as *const () as usize,
            self.sigs.ptr_u32_i64_i64_u32_to_i64,
            &[self.vm_ptr, op_v, lhs, rhs, pc_v],
        );
        self.check_exception(res);
        self.reload_regs_ptr_and_maybe_all();
        self.builder.ins().jump(merge_blk, &[res.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_strict_eq(&mut self, dst: usize, lhs_r: usize, rhs_r: usize, pc: u32, negate: bool) {
        let lhs = self.read_reg(lhs_r);
        let rhs = self.read_reg(rhs_r);
        let l_tag = self.builder.ins().band_imm_s(lhs, TAG_MASK);
        let r_tag = self.builder.ins().band_imm_s(rhs, TAG_MASK);
        let l_i = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, l_tag, TAG_INT32);
        let r_i = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, r_tag, TAG_INT32);
        let both_i = self.builder.ins().band(l_i, r_i);
        // Special (`null`/`undefined`/`bool`) and `Object` values strictly equal another value
        // iff their 64-bit NaN-boxed representations are bit-identical!
        let l_sp = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, l_tag, TAG_SPECIAL);
        let r_sp = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, r_tag, TAG_SPECIAL);
        let l_obj = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, l_tag, TAG_OBJECT);
        let r_obj = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, r_tag, TAG_OBJECT);
        let sp_or_obj = self.builder.ins().bor(l_sp, r_sp);
        let sp_or_obj = self.builder.ins().bor(sp_or_obj, l_obj);
        let sp_or_obj = self.builder.ins().bor(sp_or_obj, r_obj);
        let can_bit_eq = self.builder.ins().bor(both_i, sp_or_obj);

        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(can_bit_eq, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let cc = if negate {
            IntCC::NotEqual
        } else {
            IntCC::Equal
        };
        let cond = self.builder.ins().icmp(cc, lhs, rhs);
        let b = self.box_bool(cond);
        self.builder.ins().jump(merge_blk, &[b.into()]);

        self.builder.switch_to_block(slow_blk);
        let op_v = self.builder.ins().iconst(types::I32, 5);
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let eq_val = self.call_trampoline_5(
            rt_cmp_slow as *const () as usize,
            self.sigs.ptr_u32_i64_i64_u32_to_i64,
            &[self.vm_ptr, op_v, lhs, rhs, pc_v],
        );
        let final_v = if negate {
            let is_t = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::Equal, eq_val, VAL_TRUE);
            let f_v = self.builder.ins().iconst(types::I64, VAL_FALSE);
            let t_v = self.builder.ins().iconst(types::I64, VAL_TRUE);
            self.builder.ins().select(is_t, f_v, t_v)
        } else {
            eq_val
        };
        self.builder.ins().jump(merge_blk, &[final_v.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_abstract_eq(&mut self, dst: usize, lhs_r: usize, rhs_r: usize, pc: u32, negate: bool) {
        let lhs = self.read_reg(lhs_r);
        let rhs = self.read_reg(rhs_r);
        let l_i = self.is_int32(lhs);
        let r_i = self.is_int32(rhs);
        let both_i = self.builder.ins().band(l_i, r_i);

        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(both_i, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let cc = if negate {
            IntCC::NotEqual
        } else {
            IntCC::Equal
        };
        let cond = self.builder.ins().icmp(cc, lhs, rhs);
        let b = self.box_bool(cond);
        self.builder.ins().jump(merge_blk, &[b.into()]);

        self.builder.switch_to_block(slow_blk);
        self.flush_all_regs();
        let op_v = self.builder.ins().iconst(types::I32, 4);
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let eq_val = self.call_trampoline_5(
            rt_cmp_slow as *const () as usize,
            self.sigs.ptr_u32_i64_i64_u32_to_i64,
            &[self.vm_ptr, op_v, lhs, rhs, pc_v],
        );
        self.check_exception(eq_val);
        self.reload_regs_ptr_and_maybe_all();
        let final_v = if negate {
            let is_t = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::Equal, eq_val, VAL_TRUE);
            let f_v = self.builder.ins().iconst(types::I64, VAL_FALSE);
            let t_v = self.builder.ins().iconst(types::I64, VAL_TRUE);
            self.builder.ins().select(is_t, f_v, t_v)
        } else {
            eq_val
        };
        self.builder.ins().jump(merge_blk, &[final_v.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_not(&mut self, dst: usize, src_r: usize) {
        let src_val = self.read_reg(src_r);
        let truthy = self.emit_is_truthy(src_val);
        let falsy = self.builder.ins().icmp_imm_s(IntCC::Equal, truthy, 0);
        let boxed = self.box_bool(falsy);
        self.write_reg(dst, boxed);
    }

    fn emit_is_truthy(&mut self, v: Value) -> Value {
        let is_true = self.builder.ins().icmp_imm_s(IntCC::Equal, v, VAL_TRUE);
        let is_false = self.builder.ins().icmp_imm_s(IntCC::Equal, v, VAL_FALSE);
        let is_undef = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, v, VAL_UNDEFINED);
        let is_null = self.builder.ins().icmp_imm_s(IntCC::Equal, v, VAL_NULL);
        let is_falsy_const = self.builder.ins().bor(is_false, is_undef);
        let is_falsy_const = self.builder.ins().bor(is_falsy_const, is_null);

        let check_i32_blk = self.builder.create_block();
        let check_slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I8);

        let one = self.builder.ins().iconst(types::I8, 1);
        let zero = self.builder.ins().iconst(types::I8, 0);

        let check_falsy_blk = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_true, merge_blk, &[one.into()], check_falsy_blk, &[]);

        self.builder.switch_to_block(check_falsy_blk);
        self.builder.ins().brif(
            is_falsy_const,
            merge_blk,
            &[zero.into()],
            check_i32_blk,
            &[],
        );

        self.builder.switch_to_block(check_i32_blk);
        let is_i = self.is_int32(v);
        let i32_blk = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_i, i32_blk, &[], check_slow_blk, &[]);

        self.builder.switch_to_block(i32_blk);
        let payload = self.builder.ins().band_imm_s(v, PAYLOAD_32_MASK);
        let nz = self.builder.ins().icmp_imm_s(IntCC::NotEqual, payload, 0);
        self.builder.ins().jump(merge_blk, &[nz.into()]);

        self.builder.switch_to_block(check_slow_blk);
        let res64 = self.call_trampoline_2(
            rt_to_bool as *const () as usize,
            self.sigs.ptr_i64_to_i64,
            &[self.vm_ptr, v],
        );
        let res8 = self.builder.ins().ireduce(types::I8, res64);
        self.builder.ins().jump(merge_blk, &[res8.into()]);

        self.builder.switch_to_block(merge_blk);
        self.builder.block_params(merge_blk)[0]
    }

    // --- Baseline IC Fallback Helpers for Non-Speculated Sites ---

    fn emit_baseline_get_prop(&mut self, dst: usize, obj_r: usize, ic_idx: u32, pc: u32) {
        let obj_v = self.read_reg(obj_r);
        let p_id = self
            .builder
            .ins()
            .iconst(types::I32, i64::from(self.program_id));
        let f_id = self
            .builder
            .ins()
            .iconst(types::I32, i64::from(self.func_idx));
        let ic_v = self.builder.ins().iconst(types::I32, i64::from(ic_idx));
        let fast_res = self.call_trampoline_5(
            rt_get_prop_ic_fast as *const () as usize,
            self.sigs.ptr_i64_u32_u32_u32_to_i64,
            &[self.vm_ptr, obj_v, p_id, f_id, ic_v],
        );
        let is_miss =
            self.builder
                .ins()
                .icmp_imm_s(IntCC::Equal, fast_res, JIT_MISS_SENTINEL as i64);
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(is_miss, slow_blk, &[], merge_blk, &[fast_res.into()]);

        self.builder.switch_to_block(slow_blk);
        self.block_obj_slots.clear();
        self.flush_all_regs();
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let res = self.call_trampoline_6(
            rt_get_prop_ic as *const () as usize,
            self.sigs.ptr_i64_u32_u32_u32_u32_to_i64,
            &[self.vm_ptr, obj_v, p_id, f_id, ic_v, pc_v],
        );
        self.check_exception(res);
        self.reload_regs_ptr_and_maybe_all();
        self.builder.ins().jump(merge_blk, &[res.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_baseline_set_prop(&mut self, obj_r: usize, val_r: usize, ic_idx: u32, pc: u32) {
        self.block_obj_slots.clear();
        let obj_v = self.read_reg(obj_r);
        let val_v = self.read_reg(val_r);
        let p_id = self
            .builder
            .ins()
            .iconst(types::I32, i64::from(self.program_id));
        let f_id = self
            .builder
            .ins()
            .iconst(types::I32, i64::from(self.func_idx));
        let ic_v = self.builder.ins().iconst(types::I32, i64::from(ic_idx));
        let miss = self.call_trampoline_6(
            rt_set_prop_ic_fast as *const () as usize,
            self.sigs.ptr_i64_i64_u32_u32_u32_to_i64,
            &[self.vm_ptr, obj_v, val_v, p_id, f_id, ic_v],
        );
        let is_miss = self.builder.ins().icmp_imm_s(IntCC::NotEqual, miss, 0);
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_miss, slow_blk, &[], merge_blk, &[]);

        self.builder.switch_to_block(slow_blk);
        self.flush_all_regs();
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let res = self.call_trampoline_7(
            rt_set_prop_ic as *const () as usize,
            self.sigs.ptr_i64_i64_u32_u32_u32_u32_to_i64,
            &[self.vm_ptr, obj_v, val_v, p_id, f_id, ic_v, pc_v],
        );
        self.check_exception(res);
        self.reload_regs_ptr_and_maybe_all();
        self.builder.ins().jump(merge_blk, &[]);

        self.builder.switch_to_block(merge_blk);
    }

    fn emit_baseline_get_elem(&mut self, dst: usize, obj_r: usize, key_r: usize, pc: u32) {
        let obj_v = self.read_reg(obj_r);
        let key_v = self.read_reg(key_r);
        let fast_res = self.call_trampoline_3(
            rt_get_elem_fast as *const () as usize,
            self.sigs.ptr_i64_i64_to_i64,
            &[self.vm_ptr, obj_v, key_v],
        );
        let is_miss =
            self.builder
                .ins()
                .icmp_imm_s(IntCC::Equal, fast_res, JIT_MISS_SENTINEL as i64);
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(is_miss, slow_blk, &[], merge_blk, &[fast_res.into()]);

        self.builder.switch_to_block(slow_blk);
        self.block_obj_slots.clear();
        self.flush_all_regs();
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let res = self.call_trampoline_4(
            rt_get_elem as *const () as usize,
            self.sigs.ptr_i64_i64_u32_to_i64,
            &[self.vm_ptr, obj_v, key_v, pc_v],
        );
        self.check_exception(res);
        self.reload_regs_ptr_and_maybe_all();
        self.builder.ins().jump(merge_blk, &[res.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_baseline_set_elem(&mut self, obj_r: usize, key_r: usize, val_r: usize, pc: u32) {
        self.block_obj_slots.clear();
        let obj_v = self.read_reg(obj_r);
        let key_v = self.read_reg(key_r);
        let val_v = self.read_reg(val_r);
        let miss = self.call_trampoline_4(
            rt_set_elem_fast as *const () as usize,
            self.sigs.ptr_i64_i64_i64_to_i64,
            &[self.vm_ptr, obj_v, key_v, val_v],
        );
        let is_miss = self.builder.ins().icmp_imm_s(IntCC::NotEqual, miss, 0);
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_miss, slow_blk, &[], merge_blk, &[]);

        self.builder.switch_to_block(slow_blk);
        self.flush_all_regs();
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let res = self.call_trampoline_5(
            rt_set_elem as *const () as usize,
            self.sigs.ptr_i64_i64_i64_u32_to_i64,
            &[self.vm_ptr, obj_v, key_v, val_v, pc_v],
        );
        self.check_exception(res);
        self.reload_regs_ptr_and_maybe_all();
        self.builder.ins().jump(merge_blk, &[]);

        self.builder.switch_to_block(merge_blk);
    }
}
