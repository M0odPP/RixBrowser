//! Tier-2 Baseline Cranelift JIT & On-Stack Replacement (OSR) Compiler.
//!
//! Reference: Spec 05 §4.3 & Issue #19.
//! Lowers compact 32-bit register bytecode (`rix_js::FunctionBytecode`) directly
//! to native x86_64 / aarch64 machine code operating on 64-bit NaN-boxed `JsValue`
//! registers held in Cranelift SSA `Variable`s (native CPU hardware registers).
//!
//! Fast inline native paths:
//! - `Move`, `LoadUndef`, `LoadNull`, `LoadTrue`, `LoadFalse`, `LoadInt`, `LoadConst` (numeric/bool/null/undef)
//! - `Add`, `Sub`, `Mul` (`Int32` with hardware overflow check -> `f64`, native `f64`, mixed `Int32`/`f64`, fallback slow path for strings/BigInt/objects)
//! - `Div`, `Rem`, `Neg`, `Inc`, `Dec`
//! - Bitwise ops (`BitAnd`, `BitOr`, `BitXor`, `Shl`, `Shr`, `UShr`, `BitNot`)
//! - Relational & equality ops (`Lt`, `LtEq`, `Gt`, `GtEq`, `StrictEq`, `StrictNotEq`, `Eq`, `NotEq`, `Not`)
//! - Conditional & unconditional branches (`Jump`, `JumpIfFalse`, `JumpIfTrue`, `JumpIfNullish`, `JumpIfNotUndef`) with inline truthiness fast paths
//! - `LoopBack` with OSR entry dispatch table and amortized cooperative interrupt/GC safepoint
//! - Direct IC-accelerated `GetProp` / `SetProp` and dense-array `GetElem` / `SetElem`
//! - Full ES2024 opcode coverage via audited `extern "C"` trampolines (`rt_step_generic`).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, AbiParam, Block, Function, InstBuilder, MemFlagsData, Signature, StackSlotData,
    StackSlotKind, UserFuncName, Value,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use rix_js::bytecode::WIDE_IC_SENTINEL;
use rix_js::{ConstantEntry, FunctionBytecode, JsValue, Opcode};
use thiserror::Error;

use crate::memory::{CodeAllocError, CodePage};
use crate::trampolines::{
    rt_add_slow, rt_cmp_slow, rt_div_slow, rt_get_elem, rt_get_elem_fast, rt_get_prop_ic,
    rt_get_prop_ic_fast, rt_load_const, rt_load_this, rt_loop_safepoint, rt_mul_slow, rt_regs_ptr,
    rt_rem_slow, rt_set_elem, rt_set_elem_fast, rt_set_prop_ic, rt_set_prop_ic_fast,
    rt_step_generic, rt_sub_slow, rt_throw, rt_to_bool, JIT_EXCEPTION_SENTINEL, JIT_MISS_SENTINEL,
};

// NaN-boxing constants mirrored from `rix_js::value` (verified by compile-time assertions).
const TAG_MASK: i64 = 0xFFFF_0000_0000_0000u64 as i64;
const PAYLOAD_32_MASK: i64 = 0x0000_0000_FFFF_FFFF;
const TAG_THRESHOLD: i64 = 0xFFF8_0000_0000_0000u64 as i64;
const TAG_INT32: i64 = 0xFFF9_0000_0000_0000u64 as i64;
const CANONICAL_NAN: i64 = rix_js::CANONICAL_NAN_BITS as i64;
const VAL_UNDEFINED: i64 = JsValue::UNDEFINED.to_raw() as i64;
const VAL_NULL: i64 = JsValue::NULL.to_raw() as i64;
const VAL_FALSE: i64 = JsValue::FALSE.to_raw() as i64;
const VAL_TRUE: i64 = JsValue::TRUE.to_raw() as i64;

const _: () = {
    assert!(JsValue::from_i32(0).to_raw() == (TAG_INT32 as u64));
    assert!(JsValue::from_i32(42).to_raw() == ((TAG_INT32 as u64) | 42));
};

/// Native ABI signature of every compiled Baseline JIT function.
///
/// Arguments: `(vm: *mut JsVm, regs_ptr: *mut u64, entry_pc: u32, safepoint_mask: u32) -> u64`.
pub type BaselineEntryFn = unsafe extern "C" fn(*mut rix_js::JsVm, *mut u64, u32, u32) -> u64;

/// Failure returned when a bytecode function cannot be compiled by Cranelift.
#[derive(Debug, Error)]
pub enum JitCompileError {
    #[error("Host ISA unsupported for Cranelift JIT: {0}")]
    UnsupportedIsa(String),
    #[error("Function cannot be JIT-compiled: {0}")]
    UnsupportedFunction(&'static str),
    #[error("Cranelift compilation error: {0}")]
    Cranelift(String),
    #[error("Executable code page allocation error: {0}")]
    CodeAlloc(#[from] CodeAllocError),
}

/// Finalized native Baseline JIT artifact owning its W^X [`CodePage`].
pub struct CompiledBaseline {
    code_page: CodePage,
    pub osr_entries: Vec<u32>,
    pub reg_count: usize,
}

impl CompiledBaseline {
    /// Returns `true` if `entry_pc` is a valid entry point (`0` or a compiled OSR loop header).
    #[must_use]
    pub fn supports_entry(&self, entry_pc: usize) -> bool {
        entry_pc == 0 || self.osr_entries.contains(&(entry_pc as u32))
    }

    #[must_use]
    pub fn code_len(&self) -> usize {
        self.code_page.code_len()
    }

    /// Executes the finalized native code page starting at `entry_pc`.
    ///
    /// # Safety
    /// - `vm` must be a valid active `JsVm` pointer whose top frame matches the compiled function.
    /// - `regs_ptr` must point to at least `self.reg_count` readable/writable `u64` register slots.
    /// - `entry_pc` must satisfy `self.supports_entry(entry_pc)`.
    pub unsafe fn invoke(
        &self,
        vm: *mut rix_js::JsVm,
        regs_ptr: *mut u64,
        entry_pc: u32,
        safepoint_mask: u32,
    ) -> u64 {
        let raw_ptr = self.code_page.as_ptr();
        // SAFETY: `self.code_page` holds finalized RX machine code emitted by Cranelift for the
        // host ISA matching the exact `BaselineEntryFn` C ABI signature, and stays alive for `&self`.
        let entry: BaselineEntryFn = unsafe { std::mem::transmute(raw_ptr) };
        // SAFETY: Caller upholds `vm`, `regs_ptr`, and `entry_pc` invariants.
        unsafe { entry(vm, regs_ptr, entry_pc, safepoint_mask) }
    }
}

/// Checks whether a [`FunctionBytecode`] is eligible for Tier-2 Baseline Cranelift compilation.
fn check_eligibility(func: &FunctionBytecode) -> Result<(), JitCompileError> {
    if func.is_async || func.is_generator {
        return Err(JitCompileError::UnsupportedFunction(
            "async/generator functions execute on suspendable Tier-1 frames",
        ));
    }
    if func.code.is_empty() || func.code.len() > 8_192 || func.register_count > 128 {
        return Err(JitCompileError::UnsupportedFunction(
            "function exceeds Baseline JIT bytecode/register budget",
        ));
    }
    for inst in &func.code {
        if matches!(
            inst.opcode(),
            Some(
                Opcode::Await
                    | Opcode::Yield
                    | Opcode::YieldStar
                    | Opcode::PushWith
                    | Opcode::PopWith
            )
        ) {
            return Err(JitCompileError::UnsupportedFunction(
                "function uses Tier-1-only control opcode",
            ));
        }
    }
    Ok(())
}

/// Resolves an instruction's wide IC/constant index (and returns whether an `ExtraArg` follows).
fn resolve_wide(func: &FunctionBytecode, pc: usize, is_abx: bool) -> (u32, usize) {
    let inst = func.code[pc];
    let raw = if is_abx {
        u32::from(inst.bx())
    } else {
        u32::from(inst.c())
    };
    let sentinel = if is_abx {
        u32::from(u16::MAX)
    } else {
        u32::from(WIDE_IC_SENTINEL)
    };
    if raw == sentinel {
        if let Some(next) = func.code.get(pc + 1) {
            if next.opcode() == Some(Opcode::ExtraArg) {
                return (next.ax_val(), pc + 2);
            }
        }
    }
    (raw, pc + 1)
}

/// Compiles `func` (`program_id`, `func_idx`) to native W^X machine code via Cranelift.
pub fn compile_baseline_function(
    program_id: u32,
    func_idx: u32,
    func: &FunctionBytecode,
) -> Result<Arc<CompiledBaseline>, JitCompileError> {
    check_eligibility(func)?;

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

    // Build entry signature: (vm_ptr, regs_ptr, entry_pc: i32, safepoint_mask: i32) -> i64
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
    let has_closures = func.code.iter().any(|i| {
        matches!(
            i.opcode(),
            Some(Opcode::NewClosure | Opcode::SetUpvalue | Opcode::GetUpvalue)
        )
    });

    // Pre-scan all basic-block leaders (pc=0, jump targets, OSR loop headers, instructions after branches).
    let mut leaders = BTreeSet::new();
    let mut osr_targets = BTreeSet::new();
    leaders.insert(0usize);
    let mut scan_pc = 0usize;
    while scan_pc < func.code.len() {
        let inst = func.code[scan_pc];
        let op = inst.opcode().unwrap_or(Opcode::Nop);
        let next_pc = match op {
            Opcode::LoadConst
            | Opcode::CheckTdz
            | Opcode::GetGlobal
            | Opcode::SetGlobal
            | Opcode::DeclareGlobal
            | Opcode::TypeOfGlobal
            | Opcode::NewClosure
            | Opcode::NewRegExp => resolve_wide(func, scan_pc, true).1,
            Opcode::GetProp
            | Opcode::SetProp
            | Opcode::DefineProp
            | Opcode::GetPrivate
            | Opcode::SetPrivate
            | Opcode::DefinePrivate
            | Opcode::InPrivate
            | Opcode::CreateClass
            | Opcode::CallMethod => resolve_wide(func, scan_pc, false).1,
            _ => scan_pc + 1,
        };
        match op {
            Opcode::Jump
            | Opcode::JumpIfFalse
            | Opcode::JumpIfTrue
            | Opcode::JumpIfNullish
            | Opcode::JumpIfNotUndef => {
                let target = ((scan_pc + 1) as isize + isize::from(inst.sbx())) as usize;
                leaders.insert(target);
                leaders.insert(next_pc);
            }
            Opcode::LoopBack => {
                let target = ((scan_pc + 1) as isize + isize::from(inst.sbx())) as usize;
                leaders.insert(target);
                osr_targets.insert(target as u32);
                leaders.insert(next_pc);
            }
            Opcode::Return | Opcode::Throw => {
                leaders.insert(next_pc);
            }
            _ => {}
        }
        scan_pc = next_pc;
    }

    // Create Cranelift Blocks for every leader PC + dedicated prologue & implicit-return epilogue.
    let prologue = builder.create_block();
    builder.append_block_params_for_function_params(prologue);
    let mut pc_blocks: HashMap<usize, Block> = HashMap::new();
    for &pc in &leaders {
        if pc < func.code.len() {
            pc_blocks.insert(pc, builder.create_block());
        }
    }
    let implicit_ret_block = builder.create_block();
    let exc_exit_block = builder.create_block();

    // Declare SSA Variables:
    // 0..reg_count = bytecode registers r0..r(N-1)
    // reg_count     = live `regs_ptr` (reloaded after calls that may reallocate `vm.registers`)
    // reg_count + 1 = `loop_tick` (I32 amortized safepoint counter)
    let reg_vars: Vec<Variable> = (0..reg_count)
        .map(|_| builder.declare_var(types::I64))
        .collect();
    let regs_ptr_var = builder.declare_var(ptr_ty);
    let loop_tick_var = builder.declare_var(types::I32);

    // Pre-import reusable C ABI trampoline signatures.
    let sigs = TrampolineSigs::new(&mut builder, ptr_ty, call_conv);

    // Emit prologue: load all live registers from `regs_ptr`, then dispatch `entry_pc` (0 or OSR header!).
    builder.switch_to_block(prologue);
    let vm_ptr = builder.block_params(prologue)[0];
    let init_regs_ptr = builder.block_params(prologue)[1];
    let entry_pc_param = builder.block_params(prologue)[2];
    let safepoint_mask = builder.block_params(prologue)[3];
    builder.def_var(regs_ptr_var, init_regs_ptr);
    let zero_i32 = builder.ins().iconst(types::I32, 0);
    builder.def_var(loop_tick_var, zero_i32);

    for (i, &rv) in reg_vars.iter().enumerate() {
        let loaded = builder.ins().load(
            types::I64,
            MemFlagsData::trusted(),
            init_regs_ptr,
            (i * 8) as i32,
        );
        builder.def_var(rv, loaded);
    }

    let entry0_block = pc_blocks[&0];
    if osr_targets.is_empty() {
        builder.ins().jump(entry0_block, &[]);
    } else {
        let mut dispatch_block = builder.create_block();
        let is_zero = builder.ins().icmp_imm_s(IntCC::Equal, entry_pc_param, 0);
        builder
            .ins()
            .brif(is_zero, entry0_block, &[], dispatch_block, &[]);
        let osr_list: Vec<u32> = osr_targets.iter().copied().collect();
        for (idx, &target_pc) in osr_list.iter().enumerate() {
            builder.switch_to_block(dispatch_block);
            let target_blk = pc_blocks[&(target_pc as usize)];
            if idx + 1 == osr_list.len() {
                builder.ins().jump(target_blk, &[]);
            } else {
                let next_disp = builder.create_block();
                let matches_pc =
                    builder
                        .ins()
                        .icmp_imm_s(IntCC::Equal, entry_pc_param, i64::from(target_pc));
                builder
                    .ins()
                    .brif(matches_pc, target_blk, &[], next_disp, &[]);
                dispatch_block = next_disp;
            }
        }
    }

    // Reserve a scratch stack slot for potential future extensions.
    let _scratch =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));

    let mut emitter = Emitter {
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
        program_id,
        func_idx,
        has_closures,
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
            block_terminated = false;
        }
        if block_terminated {
            // Unreachable dead bytecode between a terminator and the next block leader.
            let op = func.code[pc].opcode().unwrap_or(Opcode::Nop);
            pc = advance_pc(func, pc, op);
            continue;
        }

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
                emitter.write_reg(a, v);
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

            // --- Inline Fast-Path Arithmetic (`Int32` w/ overflow -> `f64`, native `f64`, slow fallback) ---
            Opcode::Add => emitter.emit_add_sub_mul(a, b, c, pc as u32, ArithOp::Add),
            Opcode::Sub => emitter.emit_add_sub_mul(a, b, c, pc as u32, ArithOp::Sub),
            Opcode::Mul => emitter.emit_add_sub_mul(a, b, c, pc as u32, ArithOp::Mul),
            Opcode::Div => emitter.emit_div_rem(a, b, c, pc as u32, false),
            Opcode::Rem => emitter.emit_div_rem(a, b, c, pc as u32, true),
            Opcode::Inc => emitter.emit_inc_dec(a, b, pc as u32, true),
            Opcode::Dec => emitter.emit_inc_dec(a, b, pc as u32, false),
            Opcode::Neg => emitter.emit_neg(a, b, pc as u32),

            // --- Inline Fast-Path Bitwise ---
            Opcode::BitAnd
            | Opcode::BitOr
            | Opcode::BitXor
            | Opcode::Shl
            | Opcode::Shr
            | Opcode::UShr => emitter.emit_bitwise(op, a, b, c, pc as u32),
            Opcode::BitNot => emitter.emit_bitnot(a, b, pc as u32),

            // --- Inline Fast-Path Relational & Equality ---
            Opcode::Lt => emitter.emit_rel_cmp(a, b, c, pc as u32, RelOp::Lt),
            Opcode::LtEq => emitter.emit_rel_cmp(a, b, c, pc as u32, RelOp::Le),
            Opcode::Gt => emitter.emit_rel_cmp(a, b, c, pc as u32, RelOp::Gt),
            Opcode::GtEq => emitter.emit_rel_cmp(a, b, c, pc as u32, RelOp::Ge),
            Opcode::StrictEq => emitter.emit_strict_eq(a, b, c, pc as u32, false),
            Opcode::StrictNotEq => emitter.emit_strict_eq(a, b, c, pc as u32, true),
            Opcode::Eq => emitter.emit_abstract_eq(a, b, c, pc as u32, false),
            Opcode::NotEq => emitter.emit_abstract_eq(a, b, c, pc as u32, true),
            Opcode::Not => emitter.emit_not(a, b),

            // --- Direct IC-Accelerated Property & Dense Array Access (zero-flush fast path) ---
            Opcode::GetProp => {
                let (ic_idx, _) = resolve_wide(func, pc, false);
                let obj_v = emitter.read_reg(b);
                let p_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(program_id));
                let f_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(func_idx));
                let ic_v = emitter.builder.ins().iconst(types::I32, i64::from(ic_idx));
                let fast_res = emitter.call_trampoline_5(
                    rt_get_prop_ic_fast as *const () as usize,
                    emitter.sigs.ptr_i64_u32_u32_u32_to_i64,
                    &[emitter.vm_ptr, obj_v, p_id, f_id, ic_v],
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
                emitter.flush_all_regs();
                let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                let res = emitter.call_trampoline_6(
                    rt_get_prop_ic as *const () as usize,
                    emitter.sigs.ptr_i64_u32_u32_u32_u32_to_i64,
                    &[emitter.vm_ptr, obj_v, p_id, f_id, ic_v, pc_v],
                );
                emitter.check_exception(res);
                emitter.reload_regs_ptr_and_maybe_all();
                emitter.builder.ins().jump(merge_blk, &[res.into()]);

                emitter.builder.switch_to_block(merge_blk);
                let out = emitter.builder.block_params(merge_blk)[0];
                emitter.write_reg(a, out);
            }
            Opcode::SetProp => {
                let (ic_idx, _) = resolve_wide(func, pc, false);
                let obj_v = emitter.read_reg(a);
                let val_v = emitter.read_reg(b);
                let p_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(program_id));
                let f_id = emitter
                    .builder
                    .ins()
                    .iconst(types::I32, i64::from(func_idx));
                let ic_v = emitter.builder.ins().iconst(types::I32, i64::from(ic_idx));
                let miss = emitter.call_trampoline_6(
                    rt_set_prop_ic_fast as *const () as usize,
                    emitter.sigs.ptr_i64_i64_u32_u32_u32_to_i64,
                    &[emitter.vm_ptr, obj_v, val_v, p_id, f_id, ic_v],
                );
                let is_miss = emitter.builder.ins().icmp_imm_s(IntCC::NotEqual, miss, 0);
                let slow_blk = emitter.builder.create_block();
                let merge_blk = emitter.builder.create_block();
                emitter
                    .builder
                    .ins()
                    .brif(is_miss, slow_blk, &[], merge_blk, &[]);

                emitter.builder.switch_to_block(slow_blk);
                emitter.flush_all_regs();
                let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                let res = emitter.call_trampoline_7(
                    rt_set_prop_ic as *const () as usize,
                    emitter.sigs.ptr_i64_i64_u32_u32_u32_u32_to_i64,
                    &[emitter.vm_ptr, obj_v, val_v, p_id, f_id, ic_v, pc_v],
                );
                emitter.check_exception(res);
                emitter.reload_regs_ptr_and_maybe_all();
                emitter.builder.ins().jump(merge_blk, &[]);

                emitter.builder.switch_to_block(merge_blk);
            }
            Opcode::GetElem => {
                let obj_v = emitter.read_reg(b);
                let key_v = emitter.read_reg(c);
                let fast_res = emitter.call_trampoline_3(
                    rt_get_elem_fast as *const () as usize,
                    emitter.sigs.ptr_i64_i64_to_i64,
                    &[emitter.vm_ptr, obj_v, key_v],
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
                emitter.flush_all_regs();
                let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                let res = emitter.call_trampoline_4(
                    rt_get_elem as *const () as usize,
                    emitter.sigs.ptr_i64_i64_u32_to_i64,
                    &[emitter.vm_ptr, obj_v, key_v, pc_v],
                );
                emitter.check_exception(res);
                emitter.reload_regs_ptr_and_maybe_all();
                emitter.builder.ins().jump(merge_blk, &[res.into()]);

                emitter.builder.switch_to_block(merge_blk);
                let out = emitter.builder.block_params(merge_blk)[0];
                emitter.write_reg(a, out);
            }
            Opcode::SetElem => {
                let obj_v = emitter.read_reg(a);
                let key_v = emitter.read_reg(b);
                let val_v = emitter.read_reg(c);
                let miss = emitter.call_trampoline_4(
                    rt_set_elem_fast as *const () as usize,
                    emitter.sigs.ptr_i64_i64_i64_to_i64,
                    &[emitter.vm_ptr, obj_v, key_v, val_v],
                );
                let is_miss = emitter.builder.ins().icmp_imm_s(IntCC::NotEqual, miss, 0);
                let slow_blk = emitter.builder.create_block();
                let merge_blk = emitter.builder.create_block();
                emitter
                    .builder
                    .ins()
                    .brif(is_miss, slow_blk, &[], merge_blk, &[]);

                emitter.builder.switch_to_block(slow_blk);
                emitter.flush_all_regs();
                let pc_v = emitter.builder.ins().iconst(types::I32, pc as i64);
                let res = emitter.call_trampoline_5(
                    rt_set_elem as *const () as usize,
                    emitter.sigs.ptr_i64_i64_i64_u32_to_i64,
                    &[emitter.vm_ptr, obj_v, key_v, val_v, pc_v],
                );
                emitter.check_exception(res);
                emitter.reload_regs_ptr_and_maybe_all();
                emitter.builder.ins().jump(merge_blk, &[]);

                emitter.builder.switch_to_block(merge_blk);
            }

            // --- Control Flow & OSR Back-Edge ---
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
                let header = emitter.target_block(target);
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

            // --- All other ES2024 opcodes (`Call`, `CallMethod`, `NewObject`, `NewArray`,
            // `GetGlobal`, `SetGlobal`, `GetUpvalue`, `SetUpvalue`, `NewClosure`, `CreateClass`, etc.) ---
            _ => {
                emitter.emit_generic_step(pc as u32);
            }
        }

        pc = next_pc;
    }

    if !block_terminated {
        emitter.builder.ins().jump(implicit_ret_block, &[]);
    }

    // Implicit `return undefined` epilogue.
    emitter.builder.switch_to_block(implicit_ret_block);
    if has_closures {
        emitter.flush_all_regs();
    }
    let undef = emitter.builder.ins().iconst(types::I64, VAL_UNDEFINED);
    emitter.builder.ins().return_(&[undef]);

    // Exception unwind exit: refresh `regs_ptr` in case a throwing slow path reallocated
    // `vm.registers`, flush live SSA registers for `try`/`catch`/`finally` handlers, and
    // return `JIT_EXCEPTION_SENTINEL`.
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

    builder.seal_all_blocks();
    builder.finalize(isa.frontend_config());

    let mut ctx = Context::for_function(clif_func);
    let mut ctrl_plane = ControlPlane::default();
    let compiled = ctx
        .compile(&*isa, &mut ctrl_plane)
        .map_err(|e| JitCompileError::Cranelift(format!("{e:?}")))?;

    let code_page = CodePage::from_machine_code(compiled.code_buffer())?;
    Ok(Arc::new(CompiledBaseline {
        code_page,
        osr_entries: osr_targets.into_iter().collect(),
        reg_count,
    }))
}

fn advance_pc(func: &FunctionBytecode, pc: usize, op: Opcode) -> usize {
    match op {
        Opcode::LoadConst
        | Opcode::CheckTdz
        | Opcode::GetGlobal
        | Opcode::SetGlobal
        | Opcode::DeclareGlobal
        | Opcode::TypeOfGlobal
        | Opcode::NewClosure
        | Opcode::NewRegExp => resolve_wide(func, pc, true).1,
        Opcode::GetProp
        | Opcode::SetProp
        | Opcode::DefineProp
        | Opcode::GetPrivate
        | Opcode::SetPrivate
        | Opcode::DefinePrivate
        | Opcode::InPrivate
        | Opcode::CreateClass
        | Opcode::CallMethod => resolve_wide(func, pc, false).1,
        _ => pc + 1,
    }
}

#[derive(Clone, Copy)]
enum ArithOp {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Copy)]
enum RelOp {
    Lt,
    Le,
    Gt,
    Ge,
}

struct TrampolineSigs {
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
    ptr_i64_u32_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_i64_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
    ptr_i64_i64_u32_u32_u32_u32_to_i64: cranelift_codegen::ir::SigRef,
}

impl TrampolineSigs {
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

struct Emitter<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    vm_ptr: Value,
    safepoint_mask: Value,
    regs_ptr_var: Variable,
    loop_tick_var: Variable,
    reg_vars: &'a [Variable],
    pc_blocks: &'a HashMap<usize, Block>,
    implicit_ret_block: Block,
    exc_exit_block: Block,
    sigs: &'a TrampolineSigs,
    ptr_ty: types::Type,
    program_id: u32,
    func_idx: u32,
    has_closures: bool,
}

impl Emitter<'_, '_> {
    #[inline]
    fn read_reg(&mut self, r: usize) -> Value {
        if let Some(&v) = self.reg_vars.get(r) {
            self.builder.use_var(v)
        } else {
            self.builder.ins().iconst(types::I64, VAL_UNDEFINED)
        }
    }

    #[inline]
    fn write_reg(&mut self, r: usize, val: Value) {
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

    // --- NaN-Boxing Inline Helpers ---

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
    fn box_i32(&mut self, i: Value) -> Value {
        let ext = self.builder.ins().uextend(types::I64, i);
        self.builder.ins().bor_imm_s(ext, TAG_INT32)
    }

    #[inline]
    fn unbox_i32(&mut self, v: Value) -> Value {
        self.builder.ins().ireduce(types::I32, v)
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

    fn emit_add_sub_mul(&mut self, dst: usize, lhs_r: usize, rhs_r: usize, pc: u32, op: ArithOp) {
        let lhs = self.read_reg(lhs_r);
        let rhs = self.read_reg(rhs_r);
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

        // Fast path 1: Int32 op Int32 with exact overflow check -> f64!
        self.builder.switch_to_block(fast_i32);
        let li = self.unbox_i32(lhs);
        let ri = self.unbox_i32(rhs);
        let l64 = self.builder.ins().sextend(types::I64, li);
        let r64 = self.builder.ins().sextend(types::I64, ri);
        let res64 = match op {
            ArithOp::Add => self.builder.ins().iadd(l64, r64),
            ArithOp::Sub => self.builder.ins().isub(l64, r64),
            ArithOp::Mul => self.builder.ins().imul(l64, r64),
        };
        let res32 = self.builder.ins().ireduce(types::I32, res64);
        let back64 = self.builder.ins().sextend(types::I64, res32);
        let fits_i32 = self.builder.ins().icmp(IntCC::Equal, res64, back64);

        // Preserve `-0.0` for `Mul` (`0 * -1` -> `-0.0`!).
        let use_i32 = if matches!(op, ArithOp::Mul) {
            let is_zero = self.builder.ins().icmp_imm_s(IntCC::Equal, res32, 0);
            let xor_sign = self.builder.ins().bxor(li, ri);
            let neg_sign = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::SignedLessThan, xor_sign, 0);
            let neg_zero = self.builder.ins().band(is_zero, neg_sign);
            let not_neg_zero = self.builder.ins().icmp_imm_s(IntCC::Equal, neg_zero, 0);
            self.builder.ins().band(fits_i32, not_neg_zero)
        } else {
            fits_i32
        };

        let i32_ok_blk = self.builder.create_block();
        let i32_ovf_blk = self.builder.create_block();
        self.builder
            .ins()
            .brif(use_i32, i32_ok_blk, &[], i32_ovf_blk, &[]);

        self.builder.switch_to_block(i32_ok_blk);
        let boxed_i = self.box_i32(res32);
        self.builder.ins().jump(merge_blk, &[boxed_i.into()]);

        self.builder.switch_to_block(i32_ovf_blk);
        let lf = self.builder.ins().fcvt_from_sint(types::F64, li);
        let rf = self.builder.ins().fcvt_from_sint(types::F64, ri);
        let f_res = match op {
            ArithOp::Add => self.builder.ins().fadd(lf, rf),
            ArithOp::Sub => self.builder.ins().fsub(lf, rf),
            ArithOp::Mul => self.builder.ins().fmul(lf, rf),
        };
        let boxed_ovf = self.box_f64(f_res);
        self.builder.ins().jump(merge_blk, &[boxed_ovf.into()]);

        // Fast path 2: Numeric (Int32 or f64) mixed floating-point arithmetic!
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
        let li_f = self.unbox_i32(lhs);
        let ri_f = self.unbox_i32(rhs);
        let lf_raw = self.unbox_f64(lhs);
        let lf_from_i = self.builder.ins().fcvt_from_sint(types::F64, li_f);
        let lf_val = self.builder.ins().select(l_i32, lf_from_i, lf_raw);
        let rf_raw = self.unbox_f64(rhs);
        let rf_from_i = self.builder.ins().fcvt_from_sint(types::F64, ri_f);
        let rf_val = self.builder.ins().select(r_i32, rf_from_i, rf_raw);
        let fres = match op {
            ArithOp::Add => self.builder.ins().fadd(lf_val, rf_val),
            ArithOp::Sub => self.builder.ins().fsub(lf_val, rf_val),
            ArithOp::Mul => self.builder.ins().fmul(lf_val, rf_val),
        };
        let boxed_f = self.box_f64(fres);
        self.builder.ins().jump(merge_blk, &[boxed_f.into()]);

        // Slow path: string concatenation, BigInt, object ToPrimitive.
        self.builder.switch_to_block(slow_blk);
        self.flush_all_regs();
        let fn_addr = match op {
            ArithOp::Add => rt_add_slow as *const () as usize,
            ArithOp::Sub => rt_sub_slow as *const () as usize,
            ArithOp::Mul => rt_mul_slow as *const () as usize,
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
        if !is_rem {
            // Fast path for numeric division (`f64 / f64`).
            let l_i32 = self.is_int32(lhs);
            let r_i32 = self.is_int32(rhs);
            let l_f = self.is_float(lhs);
            let r_f = self.is_float(rhs);
            let l_num = self.builder.ins().bor(l_i32, l_f);
            let r_num = self.builder.ins().bor(r_i32, r_f);
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
            let lf = self.builder.ins().select(l_i32, li_f, lf_raw);
            let ri_f = self.builder.ins().fcvt_from_sint(types::F64, ri);
            let rf_raw = self.unbox_f64(rhs);
            let rf = self.builder.ins().select(r_i32, ri_f, rf_raw);
            let q = self.builder.ins().fdiv(lf, rf);
            let boxed = self.box_f64(q);
            self.builder.ins().jump(merge_blk, &[boxed.into()]);

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
            return;
        }
        // `%` fast path for positive Int32 operands (e.g. `i % 10`), else slow path for IEEE-754 `frem`.
        let l_i32 = self.is_int32(lhs);
        let r_i32 = self.is_int32(rhs);
        let both_i32 = self.builder.ins().band(l_i32, r_i32);
        let li = self.unbox_i32(lhs);
        let ri = self.unbox_i32(rhs);
        let l_nonneg = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, li, 0);
        let r_pos = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThan, ri, 0);
        let fast_ok = self.builder.ins().band(both_i32, l_nonneg);
        let fast_ok = self.builder.ins().band(fast_ok, r_pos);

        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(fast_ok, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let rem = self.builder.ins().srem(li, ri);
        let boxed = self.box_i32(rem);
        self.builder.ins().jump(merge_blk, &[boxed.into()]);

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

    fn emit_inc_dec(&mut self, dst: usize, src_r: usize, pc: u32, is_inc: bool) {
        let src = self.read_reg(src_r);
        let is_i = self.is_int32(src);
        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder.ins().brif(is_i, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let si = self.unbox_i32(src);
        let limit = if is_inc { i32::MAX } else { i32::MIN };
        let no_ovf = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::NotEqual, si, i64::from(limit));
        let ok_blk = self.builder.create_block();
        self.builder.ins().brif(no_ovf, ok_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(ok_blk);
        let next = self
            .builder
            .ins()
            .iadd_imm_s(si, if is_inc { 1 } else { -1 });
        let boxed = self.box_i32(next);
        self.builder.ins().jump(merge_blk, &[boxed.into()]);

        self.builder.switch_to_block(slow_blk);
        let one = self
            .builder
            .ins()
            .iconst(types::I64, JsValue::from_i32(1).to_raw() as i64);
        self.flush_all_regs();
        let fn_addr = if is_inc {
            rt_add_slow as *const () as usize
        } else {
            rt_sub_slow as *const () as usize
        };
        let pc_v = self.builder.ins().iconst(types::I32, i64::from(pc));
        let res = self.call_trampoline_4(
            fn_addr,
            self.sigs.ptr_i64_i64_u32_to_i64,
            &[self.vm_ptr, src, one, pc_v],
        );
        self.check_exception(res);
        self.reload_regs_ptr_and_maybe_all();
        self.builder.ins().jump(merge_blk, &[res.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_neg(&mut self, dst: usize, src_r: usize, pc: u32) {
        let src = self.read_reg(src_r);
        let is_i = self.is_int32(src);
        let si = self.unbox_i32(src);
        let not_zero = self.builder.ins().icmp_imm_s(IntCC::NotEqual, si, 0);
        let not_min = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::NotEqual, si, i64::from(i32::MIN));
        let fast_ok = self.builder.ins().band(is_i, not_zero);
        let fast_ok = self.builder.ins().band(fast_ok, not_min);

        let fast_blk = self.builder.create_block();
        let slow_blk = self.builder.create_block();
        let merge_blk = self.builder.create_block();
        self.builder.append_block_param(merge_blk, types::I64);
        self.builder
            .ins()
            .brif(fast_ok, fast_blk, &[], slow_blk, &[]);

        self.builder.switch_to_block(fast_blk);
        let neg = self.builder.ins().ineg(si);
        let boxed = self.box_i32(neg);
        self.builder.ins().jump(merge_blk, &[boxed.into()]);

        self.builder.switch_to_block(slow_blk);
        self.emit_generic_step(pc);
        let reloaded = self.read_reg(dst);
        self.builder.ins().jump(merge_blk, &[reloaded.into()]);

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
        let boxed = match op {
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
                // If top bit is set (>= 2^31), unsigned shift produces f64 per ES2024!
                let fits = self
                    .builder
                    .ins()
                    .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, r, 0);
                let as_i = self.box_i32(r);
                let as_f_raw = self.builder.ins().fcvt_from_uint(types::F64, r);
                let as_f = self.box_f64(as_f_raw);
                self.builder.ins().select(fits, as_i, as_f)
            }
            _ => unreachable!(),
        };
        self.builder.ins().jump(merge_blk, &[boxed.into()]);

        self.builder.switch_to_block(slow_blk);
        self.emit_generic_step(pc);
        let reloaded = self.read_reg(dst);
        self.builder.ins().jump(merge_blk, &[reloaded.into()]);

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
        let boxed = self.box_i32(inv);
        self.builder.ins().jump(merge_blk, &[boxed.into()]);

        self.builder.switch_to_block(slow_blk);
        self.emit_generic_step(pc);
        let reloaded = self.read_reg(dst);
        self.builder.ins().jump(merge_blk, &[reloaded.into()]);

        self.builder.switch_to_block(merge_blk);
        let out = self.builder.block_params(merge_blk)[0];
        self.write_reg(dst, out);
    }

    fn emit_rel_cmp(&mut self, dst: usize, lhs_r: usize, rhs_r: usize, pc: u32, op: RelOp) {
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
            RelOp::Lt => IntCC::SignedLessThan,
            RelOp::Le => IntCC::SignedLessThanOrEqual,
            RelOp::Gt => IntCC::SignedGreaterThan,
            RelOp::Ge => IntCC::SignedGreaterThanOrEqual,
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
            RelOp::Lt => FloatCC::LessThan,
            RelOp::Le => FloatCC::LessThanOrEqual,
            RelOp::Gt => FloatCC::GreaterThan,
            RelOp::Ge => FloatCC::GreaterThanOrEqual,
        };
        let cond_f = self.builder.ins().fcmp(fcc, lf, rf);
        let bf = self.box_bool(cond_f);
        self.builder.ins().jump(merge_blk, &[bf.into()]);

        self.builder.switch_to_block(slow_blk);
        self.flush_all_regs();
        let op_code = match op {
            RelOp::Lt => 0,
            RelOp::Le => 1,
            RelOp::Gt => 2,
            RelOp::Ge => 3,
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

    /// Emits a fast inline ECMAScript `ToBoolean` returning an `I8` (0 or 1).
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
}
