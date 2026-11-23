//! Tier-3 Speculative Optimizing JIT Static & Feedback Analysis Pass.
//!
//! Reference: Spec 05 §4.4 & Issue #20.
//! Performs natural-loop discovery, copy-alias propagation across intra-block `Move`
//! instructions, hoisted shape-guard & LICM property-load planning, packed-array loop
//! analysis, same-block shape-guard coalescing, and pure acyclic leaf callee eligibility.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rix_js::bytecode::WIDE_IC_SENTINEL;
use rix_js::{
    CallSiteFeedback, ConstantEntry, ElementsKind, FunctionBytecode, IcState, JsValue, JsVm, Opcode,
};

use crate::baseline::JitCompileError;
use crate::trampolines::DeoptReason;

/// Maximum bytecode instructions in a pure leaf callee eligible for Tier-3 inlining.
const MAX_INLINE_BYTECODES: usize = 36;
const MAX_INLINE_REGS: u16 = 24;

/// Speculation switches tuned per function based on previous deoptimization history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptHints {
    pub speculate_int32: bool,
    pub speculate_shapes: bool,
    pub speculate_arrays: bool,
    pub inline_calls: bool,
}

impl Default for OptHints {
    fn default() -> Self {
        Self {
            speculate_int32: true,
            speculate_shapes: true,
            speculate_arrays: true,
            inline_calls: true,
        }
    }
}

/// Metadata entry mapping a native Tier-3 guard failure back to its exact bytecode PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeoptMetadata {
    pub resume_pc: u32,
    pub reason: DeoptReason,
}

#[derive(Debug, Clone)]
pub struct HoistedObjectPlan {
    pub shape_id: u32,
    pub min_slots: u32,
    pub has_write: bool,
    pub written_offsets: BTreeSet<u32>,
    pub invariant_read_offsets: BTreeSet<u32>,
}

#[derive(Debug, Clone)]
pub struct LoopOptPlan {
    pub header_pc: usize,
    pub back_pc: usize,
    pub hoisted_objects: BTreeMap<usize, HoistedObjectPlan>,
    pub hoisted_arrays: BTreeMap<usize, ElementsKind>,
    /// Maps each `GetProp`/`SetProp`/`GetElem`/`SetElem` PC inside the loop to the
    /// canonical loop-invariant register holding the target object/array (propagating
    /// through intra-block `Move tmp, invariant_reg` copies).
    pub pc_target_reg: HashMap<usize, usize>,
}

#[derive(Debug, Clone)]
pub struct InlineCandidate {
    pub callee_raw: u64,
    pub program_id: u32,
    pub func_idx: u32,
    pub func: FunctionBytecode,
    pub ic_snapshot: Vec<IcState>,
}

pub fn resolve_wide(func: &FunctionBytecode, pc: usize, is_abx: bool) -> (u32, usize) {
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

pub fn advance_pc(func: &FunctionBytecode, pc: usize, op: Opcode) -> usize {
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

pub fn check_opt_eligibility(func: &FunctionBytecode) -> Result<(), JitCompileError> {
    if func.is_async || func.is_generator {
        return Err(JitCompileError::UnsupportedFunction(
            "async/generator functions stay on Tier-1",
        ));
    }
    if func.code.is_empty() || func.code.len() > 8_192 || func.register_count > 128 {
        return Err(JitCompileError::UnsupportedFunction(
            "function exceeds Tier-3 bytecode/register budget",
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

/// Checks whether a monomorphic callee is a pure acyclic leaf function safe to inline.
pub fn check_inline_candidate(
    vm: &JsVm,
    feedback: CallSiteFeedback,
    argc: usize,
) -> Option<InlineCandidate> {
    let CallSiteFeedback::Monomorphic {
        callee,
        program_id,
        func_idx,
    } = feedback
    else {
        return None;
    };
    let func = vm.function_bytecode(program_id, func_idx)?;
    if func.is_async
        || func.is_generator
        || !func.exception_handlers.is_empty()
        || func.code.is_empty()
        || func.code.len() > MAX_INLINE_BYTECODES
        || func.register_count > MAX_INLINE_REGS
        || usize::from(func.param_count) != argc
    {
        return None;
    }

    let ic_snapshot = vm.jit_ic_snapshot(program_id, func_idx);
    let mut scan_pc = 0usize;
    while scan_pc < func.code.len() {
        let inst = func.code[scan_pc];
        let op = inst.opcode()?;
        let next_pc = advance_pc(func, scan_pc, op);
        match op {
            Opcode::Nop
            | Opcode::ExtraArg
            | Opcode::Move
            | Opcode::LoadUndef
            | Opcode::LoadNull
            | Opcode::LoadTrue
            | Opcode::LoadFalse
            | Opcode::LoadInt
            | Opcode::LoadTdz
            | Opcode::CheckTdz
            | Opcode::Add
            | Opcode::Sub
            | Opcode::Mul
            | Opcode::Div
            | Opcode::Rem
            | Opcode::Inc
            | Opcode::Dec
            | Opcode::Neg
            | Opcode::BitAnd
            | Opcode::BitOr
            | Opcode::BitXor
            | Opcode::Shl
            | Opcode::Shr
            | Opcode::UShr
            | Opcode::BitNot
            | Opcode::Lt
            | Opcode::LtEq
            | Opcode::Gt
            | Opcode::GtEq
            | Opcode::StrictEq
            | Opcode::StrictNotEq
            | Opcode::Not
            | Opcode::Return => {}
            Opcode::LoadConst => {
                let (cidx, _) = resolve_wide(func, scan_pc, true);
                match func.constants.get(cidx as usize) {
                    Some(ConstantEntry::Int32(_) | ConstantEntry::Number(_)) => {}
                    _ => return None,
                }
            }
            Opcode::GetProp => {
                let (ic_idx, _) = resolve_wide(func, scan_pc, false);
                match ic_snapshot.get(ic_idx as usize) {
                    Some(IcState::Monomorphic(e)) if e.holder.is_none() && !e.is_accessor => {}
                    _ => return None,
                }
            }
            Opcode::Jump
            | Opcode::JumpIfFalse
            | Opcode::JumpIfTrue
            | Opcode::JumpIfNullish
            | Opcode::JumpIfNotUndef => {
                let target = ((scan_pc + 1) as isize + isize::from(inst.sbx())) as usize;
                // Only inline strictly acyclic leaf functions (no backward edges).
                if target <= scan_pc {
                    return None;
                }
            }
            _ => return None,
        }
        scan_pc = next_pc;
    }

    Some(InlineCandidate {
        callee_raw: JsValue::from_object(callee).to_raw(),
        program_id,
        func_idx,
        func: func.clone(),
        ic_snapshot,
    })
}

pub fn is_length_ic(func: &FunctionBytecode, ic_idx: usize) -> bool {
    func.ic_slots.get(ic_idx).is_some_and(|slot| {
        matches!(
            func.constants.get(slot.key_const as usize),
            Some(ConstantEntry::String(s)) if s == "length"
        )
    })
}

/// Scans forward within the current straight-line basic-block segment to coalesce multiple
/// monomorphic own-slot `GetProp`/`SetProp` checks on canonical `(canon_obj_r, shape_id)` into one guard.
#[allow(clippy::too_many_arguments)]
pub fn scan_block_object_requirements(
    func: &FunctionBytecode,
    ics: &[IcState],
    leaders: &BTreeSet<usize>,
    start_pc: usize,
    canon_obj_r: usize,
    init_aliases: &HashMap<usize, usize>,
    shape_id: u32,
    init_min_slots: u32,
    init_for_write: bool,
) -> (u32, bool) {
    let mut min_slots = init_min_slots;
    let mut for_write = init_for_write;
    let mut aliases = init_aliases.clone();
    let resolve = |r: usize, al: &HashMap<usize, usize>| al.get(&r).copied().unwrap_or(r);

    let mut pc = advance_pc(
        func,
        start_pc,
        func.code[start_pc].opcode().unwrap_or(Opcode::Nop),
    );
    while pc < func.code.len() && !leaders.contains(&pc) {
        let inst = func.code[pc];
        let op = inst.opcode().unwrap_or(Opcode::Nop);
        let next_pc = advance_pc(func, pc, op);
        let a = usize::from(inst.a());
        match op {
            Opcode::Move => {
                let src = resolve(usize::from(inst.b()), &aliases);
                aliases.retain(|_, &mut v| v != a);
                if src != a {
                    aliases.insert(a, src);
                } else {
                    aliases.remove(&a);
                }
                if a == canon_obj_r {
                    break;
                }
            }
            Opcode::GetProp => {
                let (ic_idx, _) = resolve_wide(func, pc, false);
                if resolve(usize::from(inst.b()), &aliases) == canon_obj_r {
                    if let Some(IcState::Monomorphic(e)) = ics.get(ic_idx as usize) {
                        if e.holder.is_none() && !e.is_accessor && e.shape.0 == shape_id {
                            min_slots = min_slots.max(e.offset + 1);
                        }
                    }
                }
                aliases.remove(&a);
                aliases.retain(|_, &mut v| v != a);
                if a == canon_obj_r {
                    break;
                }
            }
            Opcode::SetProp => {
                let (ic_idx, _) = resolve_wide(func, pc, false);
                if resolve(a, &aliases) == canon_obj_r {
                    if let Some(IcState::Monomorphic(e)) = ics.get(ic_idx as usize) {
                        if e.holder.is_none()
                            && !e.is_accessor
                            && e.target_shape == e.shape
                            && e.shape.0 == shape_id
                        {
                            min_slots = min_slots.max(e.offset + 1);
                            for_write = true;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                } else {
                    let safe_other = matches!(
                        ics.get(ic_idx as usize),
                        Some(IcState::Monomorphic(e))
                            if e.holder.is_none() && !e.is_accessor && e.target_shape == e.shape
                    );
                    if !safe_other {
                        break;
                    }
                }
            }
            Opcode::Jump
            | Opcode::JumpIfFalse
            | Opcode::JumpIfTrue
            | Opcode::JumpIfNullish
            | Opcode::JumpIfNotUndef
            | Opcode::LoopBack
            | Opcode::Return
            | Opcode::Throw
            | Opcode::Call
            | Opcode::CallMethod
            | Opcode::CallWithThis
            | Opcode::CallSpread
            | Opcode::Construct
            | Opcode::DeleteProp
            | Opcode::DeleteElem
            | Opcode::DefineProp
            | Opcode::SetElem => break,
            Opcode::Nop
            | Opcode::ExtraArg
            | Opcode::Debugger
            | Opcode::CheckTdz
            | Opcode::CloseUpvalues => {}
            _ => {
                aliases.remove(&a);
                aliases.retain(|_, &mut v| v != a);
                if a == canon_obj_r {
                    break;
                }
            }
        }
        pc = next_pc;
    }
    (min_slots, for_write)
}

/// Discovers natural loops and computes hoisted shape guards, LICM property loads, and
/// hoisted packed-array guards for pure loops.
pub fn analyze_loops(
    vm: &JsVm,
    func_key: (u32, u32),
    func: &FunctionBytecode,
    ics: &[IcState],
    inline_map: &HashMap<usize, InlineCandidate>,
    has_closures: bool,
    hints: OptHints,
) -> (HashMap<usize, LoopOptPlan>, Vec<(usize, usize)>) {
    let (program_id, func_idx) = func_key;
    let mut raw_loops: Vec<(usize, usize)> = Vec::new();
    let mut pc = 0usize;
    while pc < func.code.len() {
        let inst = func.code[pc];
        let op = inst.opcode().unwrap_or(Opcode::Nop);
        let next_pc = advance_pc(func, pc, op);
        if op == Opcode::LoopBack {
            let target = ((pc + 1) as isize + isize::from(inst.sbx())) as usize;
            if target < pc {
                raw_loops.push((target, pc));
            }
        }
        pc = next_pc;
    }

    let all_loop_ranges = raw_loops.clone();

    // Keep innermost non-overlapping loops keyed by header_pc (skip nested loops sharing ranges).
    raw_loops.sort_by_key(|&(h, b)| (b - h, h));
    let mut plans: HashMap<usize, LoopOptPlan> = HashMap::new();
    let mut claimed: Vec<(usize, usize)> = Vec::new();

    for (header_pc, back_pc) in raw_loops {
        if claimed
            .iter()
            .any(|&(h, b)| !(back_pc < h || header_pc > b))
        {
            continue;
        }
        if plans.contains_key(&header_pc) {
            continue;
        }

        // Reject loops that have an external jump landing inside the loop body (`header_pc < target <= back_pc`).
        let mut irreducible = false;
        let mut check_pc = 0usize;
        while check_pc < func.code.len() {
            let inst = func.code[check_pc];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(func, check_pc, op);
            if matches!(
                op,
                Opcode::Jump
                    | Opcode::JumpIfFalse
                    | Opcode::JumpIfTrue
                    | Opcode::JumpIfNullish
                    | Opcode::JumpIfNotUndef
                    | Opcode::LoopBack
            ) {
                let target = ((check_pc + 1) as isize + isize::from(inst.sbx())) as usize;
                let from_outside = check_pc < header_pc || check_pc > back_pc;
                if from_outside && target > header_pc && target <= back_pc {
                    irreducible = true;
                    break;
                }
            }
            check_pc = next_pc;
        }
        if irreducible {
            continue;
        }

        let mut modified_regs: HashSet<usize> = HashSet::new();
        let mut is_pure = true;

        let mut lpc = header_pc;
        while lpc <= back_pc {
            let inst = func.code[lpc];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(func, lpc, op);
            let a = usize::from(inst.a());

            match op {
                Opcode::Nop
                | Opcode::ExtraArg
                | Opcode::Debugger
                | Opcode::Jump
                | Opcode::JumpIfFalse
                | Opcode::JumpIfTrue
                | Opcode::JumpIfNullish
                | Opcode::JumpIfNotUndef
                | Opcode::LoopBack
                | Opcode::Return
                | Opcode::Throw
                | Opcode::CheckTdz => {}
                Opcode::CloseUpvalues => {
                    if has_closures {
                        is_pure = false;
                    }
                }
                Opcode::GetGlobal => {
                    modified_regs.insert(a);
                    let (ic_idx, _) = resolve_wide(func, lpc, true);
                    let safe_global = matches!(
                        ics.get(ic_idx as usize),
                        Some(IcState::Monomorphic(e)) if !e.is_accessor
                    );
                    if !safe_global {
                        is_pure = false;
                    }
                }
                Opcode::SetProp => {
                    let (ic_idx, _) = resolve_wide(func, lpc, false);
                    let monomorphic_in_place = matches!(
                        ics.get(ic_idx as usize),
                        Some(IcState::Monomorphic(e))
                            if e.holder.is_none() && !e.is_accessor && e.target_shape == e.shape
                    );
                    if !monomorphic_in_place {
                        is_pure = false;
                    }
                }
                Opcode::SetElem => {
                    // Pure only if backed by a known packed array kind (validated below).
                    let kind = vm.jit_elem_feedback(program_id, func_idx, lpc);
                    if !matches!(
                        kind,
                        Some(
                            ElementsKind::PackedInt32
                                | ElementsKind::PackedFloat64
                                | ElementsKind::PackedValues
                        )
                    ) {
                        is_pure = false;
                    }
                }
                Opcode::GetProp
                | Opcode::GetElem
                | Opcode::Move
                | Opcode::LoadUndef
                | Opcode::LoadNull
                | Opcode::LoadTrue
                | Opcode::LoadFalse
                | Opcode::LoadTdz
                | Opcode::LoadInt
                | Opcode::LoadConst
                | Opcode::LoadThis
                | Opcode::Add
                | Opcode::Sub
                | Opcode::Mul
                | Opcode::Div
                | Opcode::Rem
                | Opcode::Inc
                | Opcode::Dec
                | Opcode::Neg
                | Opcode::BitAnd
                | Opcode::BitOr
                | Opcode::BitXor
                | Opcode::Shl
                | Opcode::Shr
                | Opcode::UShr
                | Opcode::BitNot
                | Opcode::Lt
                | Opcode::LtEq
                | Opcode::Gt
                | Opcode::GtEq
                | Opcode::StrictEq
                | Opcode::StrictNotEq
                | Opcode::Eq
                | Opcode::NotEq
                | Opcode::Not => {
                    modified_regs.insert(a);
                }
                Opcode::Call => {
                    modified_regs.insert(a);
                    if !inline_map.contains_key(&lpc) {
                        is_pure = false;
                    }
                }
                _ => {
                    modified_regs.insert(a);
                    is_pure = false;
                }
            }
            lpc = next_pc;
        }

        if !is_pure {
            continue;
        }

        // Collect intra-loop jump targets so copy-alias tracking resets cleanly at join points.
        let mut loop_leaders: HashSet<usize> = HashSet::new();
        loop_leaders.insert(header_pc);
        let mut scan_l = header_pc;
        while scan_l <= back_pc {
            let inst = func.code[scan_l];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(func, scan_l, op);
            if matches!(
                op,
                Opcode::Jump
                    | Opcode::JumpIfFalse
                    | Opcode::JumpIfTrue
                    | Opcode::JumpIfNullish
                    | Opcode::JumpIfNotUndef
                    | Opcode::LoopBack
            ) {
                let target = ((scan_l + 1) as isize + isize::from(inst.sbx())) as usize;
                loop_leaders.insert(target);
                loop_leaders.insert(next_pc);
            }
            scan_l = next_pc;
        }

        // Collect hoisted object shape plans & hoisted packed-array plans for loop-invariant registers,
        // propagating through intra-block `Move tmp, invariant_reg` copies!
        let mut obj_candidates: BTreeMap<usize, Option<HoistedObjectPlan>> = BTreeMap::new();
        let mut arr_candidates: BTreeMap<usize, Option<ElementsKind>> = BTreeMap::new();
        let mut pc_target_reg: HashMap<usize, usize> = HashMap::new();
        let mut local_aliases: HashMap<usize, usize> = HashMap::new();

        let mut lpc = header_pc;
        while lpc <= back_pc {
            if loop_leaders.contains(&lpc) {
                local_aliases.clear();
            }
            let inst = func.code[lpc];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(func, lpc, op);
            let a = usize::from(inst.a());
            let resolve = |r: usize, al: &HashMap<usize, usize>| al.get(&r).copied().unwrap_or(r);

            match op {
                Opcode::Move => {
                    let src = resolve(usize::from(inst.b()), &local_aliases);
                    local_aliases.retain(|_, &mut v| v != a);
                    if !modified_regs.contains(&src) {
                        local_aliases.insert(a, src);
                    } else {
                        local_aliases.remove(&a);
                    }
                }
                Opcode::GetProp => {
                    let obj_r = resolve(usize::from(inst.b()), &local_aliases);
                    let (ic_idx, _) = resolve_wide(func, lpc, false);
                    if !modified_regs.contains(&obj_r) {
                        pc_target_reg.insert(lpc, obj_r);
                        if hints.speculate_shapes {
                            if let Some(IcState::Monomorphic(e)) = ics.get(ic_idx as usize) {
                                if e.holder.is_none() && !e.is_accessor {
                                    let slot = obj_candidates.entry(obj_r).or_insert_with(|| {
                                        Some(HoistedObjectPlan {
                                            shape_id: e.shape.0,
                                            min_slots: e.offset + 1,
                                            has_write: false,
                                            written_offsets: BTreeSet::new(),
                                            invariant_read_offsets: BTreeSet::new(),
                                        })
                                    });
                                    if let Some(plan) = slot {
                                        if plan.shape_id == e.shape.0 {
                                            plan.min_slots = plan.min_slots.max(e.offset + 1);
                                            plan.invariant_read_offsets.insert(e.offset);
                                        } else {
                                            *slot = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    local_aliases.remove(&a);
                    local_aliases.retain(|_, &mut v| v != a);
                }
                Opcode::SetProp => {
                    let obj_r = resolve(a, &local_aliases);
                    let (ic_idx, _) = resolve_wide(func, lpc, false);
                    if !modified_regs.contains(&obj_r) {
                        pc_target_reg.insert(lpc, obj_r);
                        if hints.speculate_shapes {
                            if let Some(IcState::Monomorphic(e)) = ics.get(ic_idx as usize) {
                                if e.holder.is_none() && !e.is_accessor && e.target_shape == e.shape
                                {
                                    let slot = obj_candidates.entry(obj_r).or_insert_with(|| {
                                        Some(HoistedObjectPlan {
                                            shape_id: e.shape.0,
                                            min_slots: e.offset + 1,
                                            has_write: true,
                                            written_offsets: BTreeSet::new(),
                                            invariant_read_offsets: BTreeSet::new(),
                                        })
                                    });
                                    if let Some(plan) = slot {
                                        if plan.shape_id == e.shape.0 {
                                            plan.min_slots = plan.min_slots.max(e.offset + 1);
                                            plan.has_write = true;
                                            plan.written_offsets.insert(e.offset);
                                        } else {
                                            *slot = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Opcode::GetElem | Opcode::SetElem => {
                    let raw_arr = if op == Opcode::GetElem {
                        usize::from(inst.b())
                    } else {
                        a
                    };
                    let arr_r = resolve(raw_arr, &local_aliases);
                    if !modified_regs.contains(&arr_r) {
                        pc_target_reg.insert(lpc, arr_r);
                        if hints.speculate_arrays {
                            let kind = vm.jit_elem_feedback(program_id, func_idx, lpc);
                            match kind {
                                Some(
                                    k @ (ElementsKind::PackedInt32
                                    | ElementsKind::PackedFloat64
                                    | ElementsKind::PackedValues),
                                ) => {
                                    let entry = arr_candidates.entry(arr_r).or_insert(Some(k));
                                    if *entry != Some(k) {
                                        *entry = None;
                                    }
                                }
                                _ => {
                                    arr_candidates.insert(arr_r, None);
                                }
                            }
                        }
                    }
                    if op == Opcode::GetElem {
                        local_aliases.remove(&a);
                        local_aliases.retain(|_, &mut v| v != a);
                    }
                }
                Opcode::Nop
                | Opcode::ExtraArg
                | Opcode::Debugger
                | Opcode::Jump
                | Opcode::JumpIfFalse
                | Opcode::JumpIfTrue
                | Opcode::JumpIfNullish
                | Opcode::JumpIfNotUndef
                | Opcode::LoopBack
                | Opcode::Return
                | Opcode::Throw
                | Opcode::CheckTdz
                | Opcode::CloseUpvalues => {}
                _ => {
                    local_aliases.remove(&a);
                    local_aliases.retain(|_, &mut v| v != a);
                }
            }
            lpc = next_pc;
        }

        // Remove any offset that is written inside the loop from `invariant_read_offsets`!
        let mut hoisted_objects = BTreeMap::new();
        for (reg, opt_plan) in obj_candidates {
            if let Some(mut plan) = opt_plan {
                for &w in &plan.written_offsets {
                    plan.invariant_read_offsets.remove(&w);
                }
                hoisted_objects.insert(reg, plan);
            }
        }
        let mut hoisted_arrays = BTreeMap::new();
        for (reg, opt_kind) in arr_candidates {
            if let Some(k) = opt_kind {
                hoisted_objects.remove(&reg);
                hoisted_arrays.insert(reg, k);
            }
        }

        // Any SetElem inside the loop MUST target a hoisted array to preserve loop purity!
        let mut set_elem_unhoisted = false;
        let mut lpc = header_pc;
        while lpc <= back_pc {
            let inst = func.code[lpc];
            let op = inst.opcode().unwrap_or(Opcode::Nop);
            let next_pc = advance_pc(func, lpc, op);
            if op == Opcode::SetElem {
                let target = pc_target_reg
                    .get(&lpc)
                    .copied()
                    .unwrap_or(usize::from(inst.a()));
                if !hoisted_arrays.contains_key(&target) {
                    set_elem_unhoisted = true;
                    break;
                }
            }
            lpc = next_pc;
        }
        if set_elem_unhoisted {
            hoisted_objects.clear();
            hoisted_arrays.clear();
        }

        if !hoisted_objects.is_empty() || !hoisted_arrays.is_empty() {
            claimed.push((header_pc, back_pc));
            plans.insert(
                header_pc,
                LoopOptPlan {
                    header_pc,
                    back_pc,
                    hoisted_objects,
                    hoisted_arrays,
                    pc_target_reg,
                },
            );
        }
    }

    (plans, all_loop_ranges)
}
