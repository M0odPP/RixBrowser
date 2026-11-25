//! Scope/Upvalue Analyzer and 32-bit Register Bytecode Compiler for `rix-js`.

use crate::ast::*;
use crate::bytecode::*;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

/// Scoping mode when compiling `eval(...)` source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalScopeMode {
    /// Normal Script or ES Module (not `eval`).
    None,
    /// Sloppy-mode top-level `eval`: `var`/`function` go to global, `let`/`const`/`class` stay local.
    GlobalVars,
    /// Strict-mode or function-nested `eval`: all declarations stay in `eval`'s local frame.
    LocalAll,
}

/// Failure returned when parsing or compiling ECMAScript source into bytecode.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CompileError {
    #[error("{0}")]
    Syntax(#[from] SyntaxError),
    #[error("CompileError: {0}")]
    Limit(String),
}

/// Compiles a parsed AST [`Program`] into register bytecode [`CompiledProgram`].
pub fn compile_program(program: &Program) -> Result<CompiledProgram, CompileError> {
    let mut compiler = Compiler::new(program.source_type);
    compiler.compile_root(program)
}

/// Compiles a parsed `eval` [`Program`] with explicit `eval` declaration scoping.
pub fn compile_eval_program(
    program: &Program,
    eval_mode: EvalScopeMode,
) -> Result<CompiledProgram, CompileError> {
    let mut compiler = Compiler::new(program.source_type);
    compiler.eval_mode = eval_mode;
    compiler.compile_root(program)
}

/// Parses and compiles ECMAScript source directly into [`CompiledProgram`].
pub fn compile(source: &str, source_type: SourceType) -> Result<CompiledProgram, CompileError> {
    let program = crate::parser::parse(source, source_type)?;
    compile_program(&program)
}

/// Collects ECMA-262 `VarDeclaredNames` (top-level `var` and top-level `FunctionDeclaration`).
pub fn collect_eval_var_declared_names(stmts: &[Statement]) -> Vec<String> {
    let mut out = Vec::new();
    collect_hoisted_vars(stmts, &mut out);
    for stmt in stmts {
        if let Statement::FunctionDecl(f) = stmt {
            if let Some(ref name) = f.name {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
struct LocalVar {
    name: String,
    reg: u8,
    depth: u32,
    is_const: bool,
    /// `true` for a named function expression's self-binding (ECMA-262 §15.2.5):
    /// immutable in all modes, throwing only in strict mode and silently ignored in sloppy mode.
    is_fn_expr_name: bool,
    is_captured: bool,
    is_tdz: bool,
    is_lex: bool,
}

#[derive(Debug, Clone)]
enum VarResolution {
    Local {
        reg: u8,
        is_const: bool,
        is_tdz: bool,
    },
    Upvalue {
        index: u8,
        is_const: bool,
        is_fn_expr_name: bool,
    },
    ModuleImport(u16),
    Global(String),
}

#[derive(Debug, Clone)]
struct LoopControl {
    label: Option<String>,
    continue_target: Option<usize>,
    continue_jumps: Vec<usize>,
    break_jumps: Vec<usize>,
    is_switch: bool,
}

struct FunctionCompiler {
    name: Option<String>,
    param_count: u16,
    length: u16,
    is_strict: bool,
    is_async: bool,
    is_generator: bool,
    is_arrow: bool,
    is_method: bool,
    code: Vec<Instruction>,
    constants: Vec<ConstantEntry>,
    string_consts: HashMap<String, u32>,
    ic_slots: Vec<IcSlotDescriptor>,
    upvalues: Vec<UpvalueDescriptor>,
    exception_handlers: Vec<ExceptionHandler>,
    locals: Vec<LocalVar>,
    scope_depth: u32,
    next_reg: u8,
    max_regs: u16,
    completion_reg: Option<u8>,
    loops: Vec<LoopControl>,
    loop_counter: u8,
    pending_label: Option<String>,
    chain_exits: Vec<Vec<(usize, u8)>>,
    active_finalizers: Vec<Vec<Statement>>,
    active_iter_closes: Vec<(u8, bool)>,
    annex_b_hoisted: HashSet<String>,
    param_names: Vec<String>,
    in_param_init: bool,
    in_class_field_or_static: bool,
    in_class_body: bool,
}

impl FunctionCompiler {
    fn new(
        name: Option<String>,
        param_count: u16,
        is_strict: bool,
        is_async: bool,
        is_generator: bool,
        is_arrow: bool,
    ) -> Self {
        Self {
            name,
            param_count,
            length: param_count,
            is_strict,
            is_async,
            is_generator,
            is_arrow,
            is_method: false,
            code: Vec::new(),
            constants: Vec::new(),
            string_consts: HashMap::new(),
            ic_slots: Vec::new(),
            upvalues: Vec::new(),
            exception_handlers: Vec::new(),
            locals: Vec::new(),
            scope_depth: 0,
            next_reg: 0,
            max_regs: 0,
            completion_reg: None,
            loops: Vec::new(),
            loop_counter: 0,
            pending_label: None,
            chain_exits: Vec::new(),
            active_finalizers: Vec::new(),
            active_iter_closes: Vec::new(),
            annex_b_hoisted: HashSet::new(),
            param_names: Vec::new(),
            in_param_init: false,
            in_class_field_or_static: false,
            in_class_body: false,
        }
    }

    fn alloc_reg(&mut self) -> Result<u8, CompileError> {
        if self.next_reg >= 250 {
            return Err(CompileError::Limit(
                "Function exceeded 250 register window limit".into(),
            ));
        }
        let r = self.next_reg;
        self.next_reg += 1;
        if u16::from(self.next_reg) > self.max_regs {
            self.max_regs = u16::from(self.next_reg);
        }
        Ok(r)
    }

    fn alloc_regs(&mut self, count: u8) -> Result<u8, CompileError> {
        if u16::from(self.next_reg) + u16::from(count) > 250 {
            return Err(CompileError::Limit(
                "Function exceeded 250 register window limit".into(),
            ));
        }
        let base = self.next_reg;
        self.next_reg += count;
        if u16::from(self.next_reg) > self.max_regs {
            self.max_regs = u16::from(self.next_reg);
        }
        Ok(base)
    }

    fn free_to(&mut self, mark: u8) {
        debug_assert!(mark <= self.next_reg);
        self.next_reg = mark;
    }

    fn add_const(&mut self, entry: ConstantEntry) -> Result<u32, CompileError> {
        if let ConstantEntry::String(ref s) = entry {
            if let Some(&idx) = self.string_consts.get(s) {
                return Ok(idx);
            }
            let key = s.clone();
            let idx = self.constants.len() as u32;
            self.constants.push(entry);
            self.string_consts.insert(key, idx);
            return Ok(idx);
        }
        let idx = self.constants.len() as u32;
        if idx >= 0x00FF_FFFF {
            return Err(CompileError::Limit("Constant pool overflow".into()));
        }
        self.constants.push(entry);
        Ok(idx)
    }

    fn add_string_const(&mut self, s: &str) -> Result<u32, CompileError> {
        if let Some(&idx) = self.string_consts.get(s) {
            return Ok(idx);
        }
        let idx = self.constants.len() as u32;
        self.constants.push(ConstantEntry::String(s.to_owned()));
        self.string_consts.insert(s.to_owned(), idx);
        Ok(idx)
    }

    fn add_ic_slot(&mut self, kind: IcKind, key_const: u32, argc: u8) -> Result<u32, CompileError> {
        let idx = self.ic_slots.len() as u32;
        if idx >= 0x00FF_FFFF {
            return Err(CompileError::Limit("Inline cache slot overflow".into()));
        }
        self.ic_slots.push(IcSlotDescriptor {
            kind,
            key_const,
            argc,
        });
        Ok(idx)
    }

    fn emit(&mut self, inst: Instruction) -> usize {
        let pc = self.code.len();
        self.code.push(inst);
        pc
    }

    fn emit_ic_abc(&mut self, op: Opcode, a: u8, b: u8, ic_idx: u32) {
        if ic_idx < u32::from(WIDE_IC_SENTINEL) {
            self.emit(Instruction::abc(op, a, b, ic_idx as u8));
        } else {
            self.emit(Instruction::abc(op, a, b, WIDE_IC_SENTINEL));
            self.emit(Instruction::ax(Opcode::ExtraArg, ic_idx));
        }
    }

    fn emit_abx_wide(&mut self, op: Opcode, a: u8, idx: u32) {
        if idx < u32::from(u16::MAX) {
            self.emit(Instruction::abx(op, a, idx as u16));
        } else {
            self.emit(Instruction::abx(op, a, u16::MAX));
            self.emit(Instruction::ax(Opcode::ExtraArg, idx));
        }
    }

    fn emit_jump(&mut self, op: Opcode, a: u8) -> usize {
        self.emit(Instruction::asbx(op, a, 0))
    }

    fn patch_jump(&mut self, jump_pc: usize, target_pc: usize) -> Result<(), CompileError> {
        let offset = target_pc as isize - (jump_pc as isize + 1);
        let sbx = i16::try_from(offset)
            .map_err(|_| CompileError::Limit("Jump offset exceeded 16-bit signed range".into()))?;
        let old = self.code[jump_pc];
        let op = old.opcode().unwrap_or(Opcode::Jump);
        self.code[jump_pc] = Instruction::asbx(op, old.a(), sbx);
        Ok(())
    }

    fn emit_loop_back(&mut self, target_pc: usize) -> Result<(), CompileError> {
        let pc = self.code.len();
        let offset = target_pc as isize - (pc as isize + 1);
        let sbx = i16::try_from(offset)
            .map_err(|_| CompileError::Limit("Loop offset exceeded 16-bit signed range".into()))?;
        let id = self.loop_counter;
        self.loop_counter = self.loop_counter.wrapping_add(1);
        self.emit(Instruction::asbx(Opcode::LoopBack, id, sbx));
        Ok(())
    }

    fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    fn end_scope(&mut self) {
        let depth = self.scope_depth;
        self.scope_depth = self.scope_depth.saturating_sub(1);
        let mut lowest_captured: Option<u8> = None;
        let mut pop_count = 0usize;
        for local in self.locals.iter().rev() {
            if local.depth < depth {
                break;
            }
            if local.is_captured {
                lowest_captured = Some(lowest_captured.map_or(local.reg, |c| c.min(local.reg)));
            }
            pop_count += 1;
        }
        if let Some(reg) = lowest_captured {
            self.emit(Instruction::abc(Opcode::CloseUpvalues, reg, 0, 0));
        }
        let new_len = self.locals.len() - pop_count;
        if let Some(first_popped) = self.locals.get(new_len) {
            let new_top = first_popped.reg;
            self.locals.truncate(new_len);
            if new_top < self.next_reg {
                self.next_reg = new_top;
            }
        }
    }

    fn declare_local(
        &mut self,
        name: &str,
        is_const: bool,
        is_tdz: bool,
    ) -> Result<u8, CompileError> {
        let reg = self.alloc_reg()?;
        self.locals.push(LocalVar {
            name: name.to_owned(),
            reg,
            depth: self.scope_depth,
            is_const,
            is_fn_expr_name: false,
            is_captured: false,
            is_tdz,
            is_lex: is_tdz || is_const,
        });
        Ok(reg)
    }

    fn find_local(&self, name: &str) -> Option<(usize, &LocalVar)> {
        self.locals
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| l.name == name)
    }

    fn add_upvalue(&mut self, desc: UpvalueDescriptor) -> Result<u8, CompileError> {
        if let Some(pos) = self
            .upvalues
            .iter()
            .position(|u| u.is_local == desc.is_local && u.index == desc.index)
        {
            return Ok(pos as u8);
        }
        if self.upvalues.len() >= 255 {
            return Err(CompileError::Limit(
                "Function exceeded 255 upvalue limit".into(),
            ));
        }
        let idx = self.upvalues.len() as u8;
        self.upvalues.push(desc);
        Ok(idx)
    }

    fn finish(self) -> FunctionBytecode {
        FunctionBytecode {
            name: self.name,
            param_count: self.param_count,
            length: self.length,
            register_count: self.max_regs.max(1),
            is_strict: self.is_strict,
            is_async: self.is_async,
            is_generator: self.is_generator,
            is_arrow: self.is_arrow,
            is_method: self.is_method,
            code: self.code,
            constants: self.constants,
            ic_slots: self.ic_slots,
            upvalues: self.upvalues,
            exception_handlers: self.exception_handlers,
        }
    }
}

struct Compiler {
    source_type: SourceType,
    eval_mode: EvalScopeMode,
    functions: Vec<FunctionBytecode>,
    frames: Vec<FunctionCompiler>,
    module: Option<ModuleBlueprint>,
    module_imports: HashMap<String, u16>,
    module_exports: HashMap<String, u16>,
}

impl Compiler {
    fn new(source_type: SourceType) -> Self {
        Self {
            source_type,
            eval_mode: EvalScopeMode::None,
            functions: Vec::new(),
            frames: Vec::new(),
            module: (source_type == SourceType::Module).then(ModuleBlueprint::default),
            module_imports: HashMap::new(),
            module_exports: HashMap::new(),
        }
    }

    #[inline]
    fn cur(&mut self) -> &mut FunctionCompiler {
        self.frames.last_mut().expect("active function frame")
    }

    fn compile_root(&mut self, program: &Program) -> Result<CompiledProgram, CompileError> {
        let is_module = program.source_type == SourceType::Module;
        let root_name = if is_module {
            "<module>"
        } else if self.eval_mode != EvalScopeMode::None {
            "<eval>"
        } else {
            "<script>"
        };
        let mut root = FunctionCompiler::new(
            Some(root_name.into()),
            0,
            program.strict,
            is_module, // modules can execute top-level await
            false,
            false,
        );
        let comp_reg = root.alloc_reg()?;
        root.emit(Instruction::abc(Opcode::LoadUndef, comp_reg, 0, 0));
        root.completion_reg = Some(comp_reg);
        self.frames.push(root);

        // Pre-pass: register module imports/exports and hoist `var` & `function` declarations.
        if is_module {
            self.collect_module_imports(&program.body);
            self.collect_module_exports(&program.body);
        }
        self.hoist_declarations(&program.body, true)?;

        for stmt in &program.body {
            self.compile_stmt(stmt)?;
        }

        let ret_reg = self.cur().completion_reg.unwrap_or(0);
        self.cur()
            .emit(Instruction::abc(Opcode::Return, ret_reg, 0, 0));
        let root_fn = self.frames.pop().expect("root frame").finish();
        let entry_idx = self.functions.len() as u32;
        self.functions.push(root_fn);

        Ok(CompiledProgram {
            bytecode_version: BYTECODE_VERSION,
            source_type: program.source_type,
            entry_func: entry_idx,
            functions: std::mem::take(&mut self.functions),
            module: self.module.take(),
        })
    }

    fn collect_module_imports(&mut self, stmts: &[Statement]) {
        let Some(ref mut module) = self.module else {
            return;
        };
        for stmt in stmts {
            if let Statement::ImportDecl(decl) = stmt {
                if decl.specifiers.is_empty() {
                    module.imports.push(ModuleImportEntry {
                        specifier: decl.source.clone(),
                        imported_name: ModuleImportKind::SideEffectOnly,
                        local_name: String::new(),
                        import_type: decl.import_type.clone(),
                    });
                    continue;
                }
                for spec in &decl.specifiers {
                    let idx = module.imports.len() as u16;
                    let (kind, local) = match spec {
                        ImportSpecifier::Default(l) => (ModuleImportKind::Default, l.clone()),
                        ImportSpecifier::Namespace(l) => (ModuleImportKind::Namespace, l.clone()),
                        ImportSpecifier::Named { imported, local } => {
                            (ModuleImportKind::Named(imported.clone()), local.clone())
                        }
                    };
                    self.module_imports.insert(local.clone(), idx);
                    module.imports.push(ModuleImportEntry {
                        specifier: decl.source.clone(),
                        imported_name: kind,
                        local_name: local,
                        import_type: decl.import_type.clone(),
                    });
                }
            }
        }
    }

    fn hoist_declarations(
        &mut self,
        stmts: &[Statement],
        is_root: bool,
    ) -> Result<(), CompileError> {
        let mut var_names = Vec::new();
        let mut func_decls = Vec::new();
        let mut top_lex_set = HashSet::new();
        collect_hoisted_vars(stmts, &mut var_names);
        for stmt in stmts {
            let inner = match stmt {
                Statement::ExportDecl(ExportDecl {
                    kind: ExportKind::Declaration(d),
                    ..
                }) => d.as_ref(),
                other => other,
            };
            match unwrap_labeled_func_decl(inner) {
                Some(f) => func_decls.push(f),
                None => match inner {
                    Statement::VarDecl {
                        kind: VarKind::Let | VarKind::Const,
                        decls,
                        ..
                    } => {
                        for d in decls {
                            let mut names = Vec::new();
                            d.id.collect_bound_names(&mut names);
                            for (n, _) in names {
                                top_lex_set.insert(n);
                            }
                        }
                    }
                    Statement::ClassDecl(cls) => {
                        if let Some(ref name) = cls.name {
                            top_lex_set.insert(name.clone());
                        }
                    }
                    _ => {}
                },
            }
        }

        let is_var_global = is_root
            && self.source_type == SourceType::Script
            && self.eval_mode != EvalScopeMode::LocalAll;
        let is_lex_global = is_root
            && self.source_type == SourceType::Script
            && self.eval_mode == EvalScopeMode::None;

        for name in &var_names {
            if is_var_global {
                let c = self.cur().add_string_const(name)?;
                let tmp = self.cur().alloc_reg()?;
                // Empty sentinel instructs DeclareGlobal to initialize `undefined` only if absent.
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadTdz, tmp, 0, 0));
                self.cur().emit_abx_wide(Opcode::DeclareGlobal, tmp, c);
                self.cur().free_to(tmp);
            } else if self.cur().find_local(name).is_none() {
                let reg = self.cur().declare_local(name, false, false)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadUndef, reg, 0, 0));
            }
        }

        // Annex B §B.3.3: hoist sloppy-mode block-level plain FunctionDeclarations to `undefined`.
        if !self.cur().is_strict && (self.source_type == SourceType::Script || !is_root) {
            let mut annex_b_names = Vec::new();
            collect_annex_b_block_funcs(stmts, &top_lex_set, false, &mut annex_b_names);
            for name in &annex_b_names {
                self.cur().annex_b_hoisted.insert(name.clone());
                if is_var_global {
                    if !var_names.contains(name)
                        && !func_decls
                            .iter()
                            .any(|f| f.name.as_deref() == Some(name.as_str()))
                    {
                        let c = self.cur().add_string_const(name)?;
                        let tmp = self.cur().alloc_reg()?;
                        self.cur()
                            .emit(Instruction::abc(Opcode::LoadTdz, tmp, 0, 0));
                        self.cur().emit_abx_wide(Opcode::DeclareGlobal, tmp, c);
                        self.cur().free_to(tmp);
                    }
                } else if self.cur().find_local(name).is_none() {
                    let reg = self.cur().declare_local(name, false, false)?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, reg, 0, 0));
                }
            }
        }

        // Pre-declare lexical (`let`/`const`/`class`) bindings in this scope as TDZ
        // BEFORE compiling hoisted functions so inner closures can capture them.
        if !is_lex_global {
            for stmt in stmts {
                let inner = match stmt {
                    Statement::ExportDecl(ExportDecl {
                        kind: ExportKind::Declaration(d),
                        ..
                    }) => d.as_ref(),
                    other => other,
                };
                match inner {
                    Statement::VarDecl {
                        kind: kind @ (VarKind::Let | VarKind::Const),
                        decls,
                        ..
                    } => {
                        for d in decls {
                            self.declare_pattern_locals(&d.id, *kind)?;
                        }
                    }
                    Statement::ClassDecl(cls) => {
                        if let Some(ref name) = cls.name {
                            let cur_depth = self.cur().scope_depth;
                            let already = self
                                .cur()
                                .locals
                                .iter()
                                .any(|l| l.depth == cur_depth && l.name == *name);
                            if !already {
                                let reg = self.cur().declare_local(name, false, true)?;
                                self.cur()
                                    .emit(Instruction::abc(Opcode::LoadTdz, reg, 0, 0));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Pre-declare ALL hoisted function names in non-script-global scopes BEFORE
        // compiling any of their bodies so sibling functions can call each other
        // (mutual recursion & forward references, ECMA-262 §10.2.11).
        if !is_var_global {
            for func in &func_decls {
                if let Some(ref name) = func.name {
                    if self.cur().find_local(name).is_none() {
                        let reg = self.cur().declare_local(name, false, false)?;
                        self.cur()
                            .emit(Instruction::abc(Opcode::LoadUndef, reg, 0, 0));
                    }
                }
            }
        }

        for func in func_decls {
            let func_idx = self.compile_function_body(func, false)?;
            let const_idx = self.cur().add_const(ConstantEntry::Function(func_idx))?;
            if let Some(ref name) = func.name {
                if is_var_global {
                    let tmp = self.cur().alloc_reg()?;
                    self.cur().emit_abx_wide(Opcode::NewClosure, tmp, const_idx);
                    let name_c = self.cur().add_string_const(name)?;
                    self.cur().emit_abx_wide(Opcode::DeclareGlobal, tmp, name_c);
                    self.cur().free_to(tmp);
                } else {
                    let reg = match self.cur().find_local(name) {
                        Some((_, l)) => l.reg,
                        None => self.cur().declare_local(name, false, false)?,
                    };
                    self.cur().emit_abx_wide(Opcode::NewClosure, reg, const_idx);
                }
            }
        }

        Ok(())
    }

    /// Implements ECMA-262 §14.2.3 `BlockDeclarationInstantiation`: pre-declares
    /// block-scoped `let`/`const`/`class` TDZ bindings and hoists block-scoped
    /// `FunctionDeclaration`s before compiling the block body.
    fn hoist_block_declarations(&mut self, stmts: &[Statement]) -> Result<(), CompileError> {
        let cur_depth = self.cur().scope_depth;
        let mut func_decls = Vec::new();
        for stmt in stmts {
            match stmt {
                Statement::VarDecl {
                    kind: kind @ (VarKind::Let | VarKind::Const),
                    decls,
                    ..
                } => {
                    for d in decls {
                        self.declare_pattern_locals(&d.id, *kind)?;
                    }
                }
                Statement::ClassDecl(cls) => {
                    if let Some(ref name) = cls.name {
                        let already = self
                            .cur()
                            .locals
                            .iter()
                            .any(|l| l.depth == cur_depth && l.name == *name);
                        if !already {
                            let reg = self.cur().declare_local(name, false, true)?;
                            self.cur()
                                .emit(Instruction::abc(Opcode::LoadTdz, reg, 0, 0));
                        }
                    }
                }
                Statement::FunctionDecl(f) => {
                    func_decls.push(f);
                }
                Statement::Labeled { .. } => {
                    if let Some(f) = unwrap_labeled_func_decl(stmt) {
                        func_decls.push(f);
                    }
                }
                _ => {}
            }
        }
        for func in &func_decls {
            if let Some(ref name) = func.name {
                let already = self
                    .cur()
                    .locals
                    .iter()
                    .any(|l| l.depth == cur_depth && l.name == *name);
                if !already {
                    let is_strict_or_special =
                        self.cur().is_strict || func.is_async || func.is_generator;
                    let reg = self.cur().declare_local(name, false, false)?;
                    if is_strict_or_special {
                        if let Some(last) = self.cur().locals.last_mut() {
                            last.is_lex = true;
                        }
                    }
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, reg, 0, 0));
                }
            }
        }
        for func in func_decls {
            let func_idx = self.compile_function_body(func, false)?;
            let const_idx = self.cur().add_const(ConstantEntry::Function(func_idx))?;
            if let Some(ref name) = func.name {
                let reg = self
                    .cur()
                    .locals
                    .iter()
                    .rev()
                    .find(|l| l.depth == cur_depth && l.name == *name)
                    .map(|l| l.reg);
                let reg = match reg {
                    Some(r) => r,
                    None => self.cur().declare_local(name, false, false)?,
                };
                self.cur().emit_abx_wide(Opcode::NewClosure, reg, const_idx);
            }
        }
        Ok(())
    }

    /// Resolves `name` starting at `frame_idx`, propagating upvalues across nested closures.
    fn resolve_var(&mut self, frame_idx: usize, name: &str) -> Result<VarResolution, CompileError> {
        if let Some((_, local)) = self.frames[frame_idx].find_local(name) {
            return Ok(VarResolution::Local {
                reg: local.reg,
                is_const: local.is_const,
                is_tdz: local.is_tdz,
            });
        }

        if frame_idx > 0 {
            let parent_res = self.resolve_var(frame_idx - 1, name)?;
            match parent_res {
                VarResolution::Local { reg, is_const, .. } => {
                    let is_fn_expr_name = if let Some((idx, flag)) = self.frames[frame_idx - 1]
                        .find_local(name)
                        .map(|(idx, l)| (idx, l.is_fn_expr_name))
                    {
                        self.frames[frame_idx - 1].locals[idx].is_captured = true;
                        flag
                    } else {
                        false
                    };
                    let uv_idx = self.frames[frame_idx].add_upvalue(UpvalueDescriptor {
                        is_local: true,
                        index: reg,
                        is_const,
                    })?;
                    return Ok(VarResolution::Upvalue {
                        index: uv_idx,
                        is_const,
                        is_fn_expr_name,
                    });
                }
                VarResolution::Upvalue {
                    index,
                    is_const,
                    is_fn_expr_name,
                } => {
                    let uv_idx = self.frames[frame_idx].add_upvalue(UpvalueDescriptor {
                        is_local: false,
                        index,
                        is_const,
                    })?;
                    return Ok(VarResolution::Upvalue {
                        index: uv_idx,
                        is_const,
                        is_fn_expr_name,
                    });
                }
                other => return Ok(other),
            }
        }

        if self.source_type == SourceType::Module {
            if let Some(&import_idx) = self.module_imports.get(name) {
                return Ok(VarResolution::ModuleImport(import_idx));
            }
        }

        Ok(VarResolution::Global(name.to_owned()))
    }

    fn emit_load_var(&mut self, name: &str, dst: u8) -> Result<(), CompileError> {
        let frame_idx = self.frames.len() - 1;
        match self.resolve_var(frame_idx, name)? {
            VarResolution::Local { reg, is_tdz, .. } => {
                if is_tdz {
                    let c = self.cur().add_string_const(name)?;
                    self.cur().emit_abx_wide(Opcode::CheckTdz, reg, c);
                }
                if dst != reg {
                    self.cur().emit(Instruction::abc(Opcode::Move, dst, reg, 0));
                }
            }
            VarResolution::Upvalue { index, .. } => {
                self.cur()
                    .emit(Instruction::abx(Opcode::GetUpvalue, dst, u16::from(index)));
            }
            VarResolution::ModuleImport(idx) => {
                self.cur()
                    .emit(Instruction::abx(Opcode::GetModuleImport, dst, idx));
            }
            VarResolution::Global(g) => {
                let c = self.cur().add_string_const(&g)?;
                let ic = self.cur().add_ic_slot(IcKind::GetGlobal, c, 0)?;
                self.cur().emit_abx_wide(Opcode::GetGlobal, dst, ic);
            }
        }
        Ok(())
    }

    fn emit_store_var(&mut self, name: &str, src: u8) -> Result<(), CompileError> {
        let frame_idx = self.frames.len() - 1;
        let res = self.resolve_var(frame_idx, name)?;
        match &res {
            VarResolution::Local {
                reg,
                is_const,
                is_tdz,
            } => {
                let (reg, is_const, is_tdz) = (*reg, *is_const, *is_tdz);
                if is_tdz {
                    let c = self.cur().add_string_const(name)?;
                    self.cur().emit_abx_wide(Opcode::CheckTdz, reg, c);
                }
                if is_const {
                    let msg = self
                        .cur()
                        .add_string_const(&format!("Assignment to constant variable '{name}'"))?;
                    let tmp = self.cur().alloc_reg()?;
                    self.cur().emit_abx_wide(Opcode::LoadConst, tmp, msg);
                    self.cur().emit(Instruction::abc(Opcode::Throw, tmp, 0, 0));
                    self.cur().free_to(tmp);
                } else if src != reg {
                    self.cur().emit(Instruction::abc(Opcode::Move, reg, src, 0));
                }
            }
            VarResolution::Upvalue {
                index,
                is_const,
                is_fn_expr_name,
            } => {
                let (index, is_const, is_fn_expr_name) = (*index, *is_const, *is_fn_expr_name);
                if is_fn_expr_name && !self.cur().is_strict {
                    // Sloppy-mode assignment to named function expression binding is silently ignored.
                } else if is_const {
                    let msg = self
                        .cur()
                        .add_string_const(&format!("Assignment to constant variable '{name}'"))?;
                    let tmp = self.cur().alloc_reg()?;
                    self.cur().emit_abx_wide(Opcode::LoadConst, tmp, msg);
                    self.cur().emit(Instruction::abc(Opcode::Throw, tmp, 0, 0));
                    self.cur().free_to(tmp);
                } else {
                    self.cur()
                        .emit(Instruction::abx(Opcode::SetUpvalue, src, u16::from(index)));
                }
            }
            VarResolution::ModuleImport(_) => {
                let msg = self
                    .cur()
                    .add_string_const(&format!("Assignment to imported binding '{name}'"))?;
                let tmp = self.cur().alloc_reg()?;
                self.cur().emit_abx_wide(Opcode::LoadConst, tmp, msg);
                self.cur().emit(Instruction::abc(Opcode::Throw, tmp, 0, 0));
                self.cur().free_to(tmp);
            }
            VarResolution::Global(g) => {
                let g_name = g.clone();
                let c = self.cur().add_string_const(&g_name)?;
                let ic = self.cur().add_ic_slot(IcKind::SetGlobal, c, 0)?;
                self.cur().emit_abx_wide(Opcode::SetGlobal, src, ic);
            }
        }
        // Propagate live module export binding update when writing a module-root binding
        // either directly at top-level or via an upvalue from an inner module function.
        if let Some(&export_idx) = self.module_exports.get(name) {
            let writes_root =
                self.frames.len() == 1 || matches!(res, VarResolution::Upvalue { .. });
            if writes_root {
                self.cur()
                    .emit(Instruction::abx(Opcode::SetModuleExport, src, export_idx));
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Statement Compilation
    // -----------------------------------------------------------------------

    fn compile_stmt(&mut self, stmt: &Statement) -> Result<(), CompileError> {
        let label = self.cur().pending_label.take();
        match stmt {
            Statement::Empty { .. } => {}
            Statement::Debugger { .. } => {
                self.cur().emit(Instruction::abc(Opcode::Debugger, 0, 0, 0));
            }
            Statement::Block { body, .. } => {
                if let Some(lbl) = label {
                    self.cur().loops.push(LoopControl {
                        label: Some(lbl),
                        continue_target: None,
                        continue_jumps: Vec::new(),
                        break_jumps: Vec::new(),
                        is_switch: true,
                    });
                    self.cur().begin_scope();
                    self.hoist_block_declarations(body)?;
                    for s in body {
                        self.compile_stmt(s)?;
                    }
                    self.cur().end_scope();
                    let ctrl = self.cur().loops.pop().expect("label frame");
                    let end_pc = self.cur().code.len();
                    for j in ctrl.break_jumps {
                        self.cur().patch_jump(j, end_pc)?;
                    }
                } else {
                    self.cur().begin_scope();
                    self.hoist_block_declarations(body)?;
                    for s in body {
                        self.compile_stmt(s)?;
                    }
                    self.cur().end_scope();
                }
            }
            Statement::Expr { expr, .. } => {
                let saved = self.cur().next_reg;
                if let Some(dst) = self.cur().completion_reg {
                    self.compile_expr(expr, dst)?;
                } else {
                    let r = self.cur().alloc_reg()?;
                    self.compile_expr(expr, r)?;
                }
                self.cur().free_to(saved);
            }
            Statement::VarDecl { kind, decls, .. } => {
                self.compile_var_decl(*kind, decls)?;
            }
            Statement::If {
                test,
                consequent,
                alternate,
                ..
            } => {
                let saved = self.cur().next_reg;
                let cond_reg = self.cur().alloc_reg()?;
                self.compile_expr(test, cond_reg)?;
                let jmp_false = self.cur().emit_jump(Opcode::JumpIfFalse, cond_reg);
                self.cur().free_to(saved);

                self.compile_stmt(consequent)?;
                if let Some(alt) = alternate {
                    let jmp_end = self.cur().emit_jump(Opcode::Jump, 0);
                    let else_pc = self.cur().code.len();
                    self.cur().patch_jump(jmp_false, else_pc)?;
                    self.compile_stmt(alt)?;
                    let end_pc = self.cur().code.len();
                    self.cur().patch_jump(jmp_end, end_pc)?;
                } else {
                    let end_pc = self.cur().code.len();
                    self.cur().patch_jump(jmp_false, end_pc)?;
                }
            }
            Statement::While { test, body, .. } => {
                if let Some(comp) = self.cur().completion_reg {
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, comp, 0, 0));
                }
                let loop_start = self.cur().code.len();
                self.cur().loops.push(LoopControl {
                    label,
                    continue_target: Some(loop_start),
                    continue_jumps: Vec::new(),
                    break_jumps: Vec::new(),
                    is_switch: false,
                });

                let saved = self.cur().next_reg;
                let cond_reg = self.cur().alloc_reg()?;
                self.compile_expr(test, cond_reg)?;
                let jmp_exit = self.cur().emit_jump(Opcode::JumpIfFalse, cond_reg);
                self.cur().free_to(saved);

                self.compile_stmt(body)?;
                self.cur().emit_loop_back(loop_start)?;

                let end_pc = self.cur().code.len();
                self.cur().patch_jump(jmp_exit, end_pc)?;
                let ctrl = self.cur().loops.pop().expect("loop frame");
                for j in ctrl.break_jumps {
                    self.cur().patch_jump(j, end_pc)?;
                }
            }
            Statement::DoWhile { body, test, .. } => {
                if let Some(comp) = self.cur().completion_reg {
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, comp, 0, 0));
                }
                let loop_start = self.cur().code.len();
                self.cur().loops.push(LoopControl {
                    label,
                    continue_target: None,
                    continue_jumps: Vec::new(),
                    break_jumps: Vec::new(),
                    is_switch: false,
                });

                self.compile_stmt(body)?;
                let cond_pc = self.cur().code.len();
                let saved = self.cur().next_reg;
                let cond_reg = self.cur().alloc_reg()?;
                self.compile_expr(test, cond_reg)?;
                let jmp_exit = self.cur().emit_jump(Opcode::JumpIfFalse, cond_reg);
                self.cur().free_to(saved);
                self.cur().emit_loop_back(loop_start)?;

                let end_pc = self.cur().code.len();
                self.cur().patch_jump(jmp_exit, end_pc)?;
                let ctrl = self.cur().loops.pop().expect("loop frame");
                for j in ctrl.continue_jumps {
                    self.cur().patch_jump(j, cond_pc)?;
                }
                for j in ctrl.break_jumps {
                    self.cur().patch_jump(j, end_pc)?;
                }
            }
            Statement::For {
                init,
                test,
                update,
                body,
                ..
            } => {
                if let Some(comp) = self.cur().completion_reg {
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, comp, 0, 0));
                }
                self.cur().begin_scope();
                let mut per_iter_close_reg: Option<u8> = None;
                if let Some(init) = init {
                    match init {
                        ForInit::VarDecl { kind, decls, .. } => {
                            let start_reg = self.cur().next_reg;
                            self.compile_var_decl(*kind, decls)?;
                            if matches!(kind, VarKind::Let | VarKind::Const) {
                                per_iter_close_reg = Some(start_reg);
                            }
                        }
                        ForInit::Expr(e) => {
                            let saved = self.cur().next_reg;
                            let r = self.cur().alloc_reg()?;
                            self.compile_expr(e, r)?;
                            self.cur().free_to(saved);
                        }
                    }
                }
                let loop_start = self.cur().code.len();
                self.cur().loops.push(LoopControl {
                    label,
                    continue_target: None,
                    continue_jumps: Vec::new(),
                    break_jumps: Vec::new(),
                    is_switch: false,
                });

                let jmp_exit = if let Some(test) = test {
                    let saved = self.cur().next_reg;
                    let cond_reg = self.cur().alloc_reg()?;
                    self.compile_expr(test, cond_reg)?;
                    let j = self.cur().emit_jump(Opcode::JumpIfFalse, cond_reg);
                    self.cur().free_to(saved);
                    Some(j)
                } else {
                    None
                };

                self.compile_stmt(body)?;
                let update_pc = self.cur().code.len();
                if let Some(close_reg) = per_iter_close_reg {
                    self.cur()
                        .emit(Instruction::abc(Opcode::CloseUpvalues, close_reg, 0, 0));
                }
                if let Some(upd) = update {
                    let saved = self.cur().next_reg;
                    let r = self.cur().alloc_reg()?;
                    self.compile_expr(upd, r)?;
                    self.cur().free_to(saved);
                }
                self.cur().emit_loop_back(loop_start)?;

                let end_pc = self.cur().code.len();
                if let Some(j) = jmp_exit {
                    self.cur().patch_jump(j, end_pc)?;
                }
                let ctrl = self.cur().loops.pop().expect("loop frame");
                for j in ctrl.continue_jumps {
                    self.cur().patch_jump(j, update_pc)?;
                }
                for j in ctrl.break_jumps {
                    self.cur().patch_jump(j, end_pc)?;
                }
                self.cur().end_scope();
            }
            Statement::ForIn {
                left, right, body, ..
            } => {
                if let Some(comp) = self.cur().completion_reg {
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, comp, 0, 0));
                }
                self.compile_for_each(label, left, right, body, ForEachKind::In)?;
            }
            Statement::ForOf {
                is_await,
                left,
                right,
                body,
                ..
            } => {
                if let Some(comp) = self.cur().completion_reg {
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, comp, 0, 0));
                }
                let kind = if *is_await {
                    if self.frames.len() == 1 {
                        if let Some(ref mut m) = self.module {
                            m.has_top_level_await = true;
                        }
                    }
                    ForEachKind::AsyncOf
                } else {
                    ForEachKind::Of
                };
                self.compile_for_each(label, left, right, body, kind)?;
            }
            Statement::Switch {
                discriminant,
                cases,
                ..
            } => {
                self.cur().begin_scope();
                let disc_reg = self.cur().alloc_reg()?;
                self.compile_expr(discriminant, disc_reg)?;
                let mut case_stmts = Vec::new();
                for c in cases {
                    case_stmts.extend(c.consequent.iter().cloned());
                }
                self.hoist_block_declarations(&case_stmts)?;
                self.cur().loops.push(LoopControl {
                    label,
                    continue_target: None,
                    continue_jumps: Vec::new(),
                    break_jumps: Vec::new(),
                    is_switch: true,
                });

                let mut case_jumps = Vec::with_capacity(cases.len());
                let mut default_idx = None;
                for (idx, case) in cases.iter().enumerate() {
                    if let Some(ref test_expr) = case.test {
                        let saved = self.cur().next_reg;
                        let test_reg = self.cur().alloc_reg()?;
                        let cmp_reg = self.cur().alloc_reg()?;
                        self.compile_expr(test_expr, test_reg)?;
                        self.cur().emit(Instruction::abc(
                            Opcode::StrictEq,
                            cmp_reg,
                            disc_reg,
                            test_reg,
                        ));
                        let j = self.cur().emit_jump(Opcode::JumpIfTrue, cmp_reg);
                        self.cur().free_to(saved);
                        case_jumps.push((idx, j));
                    } else {
                        default_idx = Some(idx);
                    }
                }
                let default_or_end_jump = self.cur().emit_jump(Opcode::Jump, 0);

                let mut case_pcs = vec![0usize; cases.len()];
                for (idx, case) in cases.iter().enumerate() {
                    case_pcs[idx] = self.cur().code.len();
                    for s in &case.consequent {
                        self.compile_stmt(s)?;
                    }
                }

                let end_pc = self.cur().code.len();
                for (idx, j) in case_jumps {
                    self.cur().patch_jump(j, case_pcs[idx])?;
                }
                let def_target = default_idx.map_or(end_pc, |i| case_pcs[i]);
                self.cur().patch_jump(default_or_end_jump, def_target)?;

                let ctrl = self.cur().loops.pop().expect("switch frame");
                for j in ctrl.break_jumps {
                    self.cur().patch_jump(j, end_pc)?;
                }
                self.cur().end_scope();
            }
            Statement::Break { label: target, .. } => {
                let idx = self.find_loop_index(target.as_deref(), true)?;
                let j = self.cur().emit_jump(Opcode::Jump, 0);
                self.cur().loops[idx].break_jumps.push(j);
            }
            Statement::Continue { label: target, .. } => {
                let idx = self.find_loop_index(target.as_deref(), false)?;
                if let Some(cont_pc) = self.cur().loops[idx].continue_target {
                    self.cur().emit_loop_back(cont_pc)?;
                } else {
                    let j = self.cur().emit_jump(Opcode::Jump, 0);
                    self.cur().loops[idx].continue_jumps.push(j);
                }
            }
            Statement::Return { arg, .. } => {
                let saved = self.cur().next_reg;
                let r = self.cur().alloc_reg()?;
                if let Some(expr) = arg {
                    self.compile_expr(expr, r)?;
                } else {
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, r, 0, 0));
                }
                if !self.cur().active_iter_closes.is_empty() {
                    let iters = self.cur().active_iter_closes.clone();
                    for &(iter_reg, is_async) in iters.iter().rev() {
                        if is_async {
                            self.cur().emit(Instruction::abc(
                                Opcode::IteratorClose,
                                iter_reg,
                                0,
                                1,
                            ));
                            self.cur()
                                .emit(Instruction::abc(Opcode::Await, iter_reg, iter_reg, 0));
                        } else {
                            self.cur().emit(Instruction::abc(
                                Opcode::IteratorClose,
                                iter_reg,
                                0,
                                0,
                            ));
                        }
                    }
                }
                if !self.cur().active_finalizers.is_empty() {
                    let fins = std::mem::take(&mut self.cur().active_finalizers);
                    for fin_body in fins.iter().rev() {
                        self.cur().begin_scope();
                        for s in fin_body {
                            self.compile_stmt(s)?;
                        }
                        self.cur().end_scope();
                    }
                    self.cur().active_finalizers = fins;
                }
                self.cur().emit(Instruction::abc(Opcode::Return, r, 0, 0));
                self.cur().free_to(saved);
            }
            Statement::Throw { arg, .. } => {
                let saved = self.cur().next_reg;
                let r = self.cur().alloc_reg()?;
                self.compile_expr(arg, r)?;
                self.cur().emit(Instruction::abc(Opcode::Throw, r, 0, 0));
                self.cur().free_to(saved);
            }
            Statement::Try {
                block,
                handler,
                finalizer,
                ..
            } => {
                if let Some(fin_body) = finalizer {
                    self.cur().active_finalizers.push(fin_body.clone());
                }
                let try_start = self.cur().code.len() as u32;
                self.cur().begin_scope();
                self.hoist_block_declarations(block)?;
                for s in block {
                    self.compile_stmt(s)?;
                }
                self.cur().end_scope();
                let try_end = self.cur().code.len() as u32;

                let jmp_after_try = self.cur().emit_jump(Opcode::Jump, 0);
                let mut catch_done_jump = None;

                if let Some(catch) = handler {
                    let catch_pc = self.cur().code.len() as u32;
                    self.cur().begin_scope();
                    let err_reg = self.cur().alloc_reg()?;
                    self.cur().exception_handlers.push(ExceptionHandler {
                        try_start,
                        try_end,
                        handler_pc: catch_pc,
                        catch_reg: err_reg,
                        is_finally: false,
                    });
                    if let Some(ref pat) = catch.param {
                        self.declare_pattern_locals(pat, VarKind::Let)?;
                        // Annex B §B.3.5: simple catch parameter does not block outer var/Annex-B.
                        if matches!(pat, Pattern::Ident { .. }) {
                            if let Some(last) = self.cur().locals.last_mut() {
                                last.is_lex = false;
                            }
                        }
                        self.compile_pattern_bind(pat, err_reg)?;
                    }
                    self.hoist_block_declarations(&catch.body)?;
                    for s in &catch.body {
                        self.compile_stmt(s)?;
                    }
                    self.cur().end_scope();
                    catch_done_jump = Some(self.cur().emit_jump(Opcode::Jump, 0));
                }

                if finalizer.is_some() {
                    self.cur().active_finalizers.pop();
                }

                let after_catch_pc = self.cur().code.len();
                self.cur().patch_jump(jmp_after_try, after_catch_pc)?;
                if let Some(j) = catch_done_jump {
                    self.cur().patch_jump(j, after_catch_pc)?;
                }

                if let Some(fin_body) = finalizer {
                    // Normal path runs finally inline; abnormal path catches, runs finally, rethrows.
                    self.cur().begin_scope();
                    self.hoist_block_declarations(fin_body)?;
                    for s in fin_body {
                        self.compile_stmt(s)?;
                    }
                    self.cur().end_scope();
                    let skip_rethrow = self.cur().emit_jump(Opcode::Jump, 0);

                    let fin_handler_pc = self.cur().code.len() as u32;
                    let saved = self.cur().next_reg;
                    let exc_reg = self.cur().alloc_reg()?;
                    self.cur().exception_handlers.push(ExceptionHandler {
                        try_start,
                        try_end: after_catch_pc as u32,
                        handler_pc: fin_handler_pc,
                        catch_reg: exc_reg,
                        is_finally: true,
                    });
                    self.cur().begin_scope();
                    self.hoist_block_declarations(fin_body)?;
                    for s in fin_body {
                        self.compile_stmt(s)?;
                    }
                    self.cur().end_scope();
                    self.cur()
                        .emit(Instruction::abc(Opcode::Throw, exc_reg, 0, 0));
                    self.cur().free_to(saved);

                    let end_pc = self.cur().code.len();
                    self.cur().patch_jump(skip_rethrow, end_pc)?;
                }
            }
            Statement::With { object, body, .. } => {
                let saved = self.cur().next_reg;
                let obj_reg = self.cur().alloc_reg()?;
                self.compile_expr(object, obj_reg)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::PushWith, obj_reg, 0, 0));
                self.compile_stmt(body)?;
                self.cur().emit(Instruction::abc(Opcode::PopWith, 0, 0, 0));
                self.cur().free_to(saved);
            }
            Statement::Labeled { label, body, .. } => {
                self.cur().pending_label = Some(label.clone());
                self.compile_stmt(body)?;
            }
            Statement::FunctionDecl(func) => {
                // Annex B §B.3.3: copy the block-local function binding's current value to the
                // enclosing non-strict function/script var binding at statement evaluation time.
                let cur_depth = self.cur().scope_depth;
                if cur_depth > 0 && !self.cur().is_strict && !func.is_async && !func.is_generator {
                    if let Some(ref name) = func.name {
                        if self.cur().annex_b_hoisted.contains(name) {
                            let blocked_by_lex = self.cur().locals.iter().any(|l| {
                                l.depth > 0 && l.depth < cur_depth && l.name == *name && l.is_lex
                            });
                            if !blocked_by_lex {
                                if let Some(block_reg) = self
                                    .cur()
                                    .locals
                                    .iter()
                                    .rev()
                                    .find(|l| l.depth == cur_depth && l.name == *name)
                                    .map(|l| l.reg)
                                {
                                    if let Some(outer_reg) = self
                                        .cur()
                                        .locals
                                        .iter()
                                        .find(|l| l.depth == 0 && l.name == *name)
                                        .map(|l| l.reg)
                                    {
                                        if outer_reg != block_reg {
                                            self.cur().emit(Instruction::abc(
                                                Opcode::Move,
                                                outer_reg,
                                                block_reg,
                                                0,
                                            ));
                                        }
                                    } else if self.frames.len() == 1
                                        && self.source_type == SourceType::Script
                                        && self.eval_mode != EvalScopeMode::LocalAll
                                    {
                                        let c = self.cur().add_string_const(name)?;
                                        self.cur().emit_abx_wide(
                                            Opcode::DeclareGlobal,
                                            block_reg,
                                            c,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Statement::ClassDecl(cls) => {
                let name = cls.name.clone().unwrap_or_default();
                let dst = if self.frames.len() == 1
                    && self.source_type == SourceType::Script
                    && self.eval_mode == EvalScopeMode::None
                    && self.cur().scope_depth == 0
                {
                    let r = self.cur().alloc_reg()?;
                    self.compile_class(cls, r)?;
                    let c = self.cur().add_string_const(&name)?;
                    self.cur().emit_abx_wide(Opcode::DeclareGlobal, r, c);
                    self.cur().free_to(r);
                    return Ok(());
                } else {
                    let cur_depth = self.cur().scope_depth;
                    if let Some((idx, l)) = self
                        .cur()
                        .locals
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, l)| l.depth == cur_depth && l.name == name)
                    {
                        let reg = l.reg;
                        self.cur().locals[idx].is_tdz = false;
                        reg
                    } else {
                        self.cur().declare_local(&name, false, false)?
                    }
                };
                self.compile_class(cls, dst)?;
            }
            Statement::ImportDecl(_) => {
                // Static imports collected during module pre-pass.
            }
            Statement::ExportDecl(export) => self.compile_export(export)?,
        }
        Ok(())
    }

    fn compile_export(&mut self, export: &ExportDecl) -> Result<(), CompileError> {
        match &export.kind {
            ExportKind::Declaration(inner) => {
                self.compile_stmt(inner)?;
                let mut names = Vec::new();
                match inner.as_ref() {
                    Statement::VarDecl { decls, .. } => {
                        for d in decls {
                            d.id.collect_bound_names(&mut names);
                        }
                    }
                    Statement::FunctionDecl(f) => {
                        if let Some(ref n) = f.name {
                            names.push((n.clone(), f.span));
                        }
                    }
                    Statement::ClassDecl(c) => {
                        if let Some(ref n) = c.name {
                            names.push((n.clone(), c.span));
                        }
                    }
                    _ => {}
                }
                for (name, _) in names {
                    let idx =
                        self.register_module_export(Some(name.clone()), name.clone(), None, false);
                    let saved = self.cur().next_reg;
                    let tmp = self.cur().alloc_reg()?;
                    self.emit_load_var(&name, tmp)?;
                    self.cur()
                        .emit(Instruction::abx(Opcode::SetModuleExport, tmp, idx));
                    self.cur().free_to(saved);
                }
            }
            ExportKind::Named { specifiers, source } => {
                for spec in specifiers {
                    let idx = self.register_module_export(
                        if source.is_none() {
                            Some(spec.local.clone())
                        } else {
                            None
                        },
                        spec.exported.clone(),
                        source.clone(),
                        false,
                    );
                    if source.is_none() {
                        let saved = self.cur().next_reg;
                        let tmp = self.cur().alloc_reg()?;
                        self.emit_load_var(&spec.local, tmp)?;
                        self.cur()
                            .emit(Instruction::abx(Opcode::SetModuleExport, tmp, idx));
                        self.cur().free_to(saved);
                    }
                }
            }
            ExportKind::All { exported, source } => {
                self.register_module_export(
                    None,
                    exported.clone().unwrap_or_else(|| "*".into()),
                    Some(source.clone()),
                    exported.is_none(),
                );
            }
            ExportKind::DefaultExpr(expr) => {
                let idx = self.register_module_export(None, "default".into(), None, false);
                let saved = self.cur().next_reg;
                let tmp = self.cur().alloc_reg()?;
                self.compile_expr(expr, tmp)?;
                self.cur()
                    .emit(Instruction::abx(Opcode::SetModuleExport, tmp, idx));
                self.cur().free_to(saved);
            }
            ExportKind::DefaultFunction(func) => {
                let idx =
                    self.register_module_export(func.name.clone(), "default".into(), None, false);
                let fn_idx = self.compile_function_body(func, false)?;
                let c = self.cur().add_const(ConstantEntry::Function(fn_idx))?;
                let saved = self.cur().next_reg;
                let tmp = self.cur().alloc_reg()?;
                self.cur().emit_abx_wide(Opcode::NewClosure, tmp, c);
                self.cur()
                    .emit(Instruction::abx(Opcode::SetModuleExport, tmp, idx));
                self.cur().free_to(saved);
            }
            ExportKind::DefaultClass(cls) => {
                let idx =
                    self.register_module_export(cls.name.clone(), "default".into(), None, false);
                let saved = self.cur().next_reg;
                let tmp = self.cur().alloc_reg()?;
                self.compile_class(cls, tmp)?;
                self.cur()
                    .emit(Instruction::abx(Opcode::SetModuleExport, tmp, idx));
                self.cur().free_to(saved);
            }
        }
        Ok(())
    }

    fn collect_module_exports(&mut self, stmts: &[Statement]) {
        for stmt in stmts {
            let Statement::ExportDecl(export) = stmt else {
                continue;
            };
            match &export.kind {
                ExportKind::Declaration(inner) => {
                    let mut names = Vec::new();
                    match inner.as_ref() {
                        Statement::VarDecl { decls, .. } => {
                            for d in decls {
                                d.id.collect_bound_names(&mut names);
                            }
                        }
                        Statement::FunctionDecl(f) => {
                            if let Some(ref n) = f.name {
                                names.push((n.clone(), f.span));
                            }
                        }
                        Statement::ClassDecl(c) => {
                            if let Some(ref n) = c.name {
                                names.push((n.clone(), c.span));
                            }
                        }
                        _ => {}
                    }
                    for (name, _) in names {
                        self.register_module_export(Some(name.clone()), name, None, false);
                    }
                }
                ExportKind::Named {
                    specifiers,
                    source: None,
                } => {
                    for spec in specifiers {
                        self.register_module_export(
                            Some(spec.local.clone()),
                            spec.exported.clone(),
                            None,
                            false,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn register_module_export(
        &mut self,
        local_name: Option<String>,
        exported_name: String,
        reexport_from: Option<String>,
        is_star: bool,
    ) -> u16 {
        let Some(ref mut module) = self.module else {
            return 0;
        };
        if reexport_from.is_none() && !is_star {
            if let Some(pos) = module
                .exports
                .iter()
                .position(|e| e.reexport_from.is_none() && e.exported_name == exported_name)
            {
                if let Some(ref local) = local_name {
                    self.module_exports.insert(local.clone(), pos as u16);
                }
                return pos as u16;
            }
        }
        let idx = module.exports.len() as u16;
        if let Some(ref local) = local_name {
            self.module_exports.insert(local.clone(), idx);
        }
        module.exports.push(ModuleExportEntry {
            local_name,
            exported_name,
            reexport_from,
            is_star,
        });
        idx
    }

    fn find_loop_index(
        &self,
        label: Option<&str>,
        allow_switch: bool,
    ) -> Result<usize, CompileError> {
        let loops = &self.frames.last().expect("frame").loops;
        for (i, l) in loops.iter().enumerate().rev() {
            if let Some(target) = label {
                if l.label.as_deref() == Some(target) {
                    return Ok(i);
                }
            } else if allow_switch || !l.is_switch {
                return Ok(i);
            }
        }
        Err(CompileError::Limit(
            "Unresolved break/continue target".into(),
        ))
    }

    fn compile_for_each(
        &mut self,
        label: Option<String>,
        left: &ForEachLeft,
        right: &Expr,
        body: &Statement,
        kind: ForEachKind,
    ) -> Result<(), CompileError> {
        self.cur().begin_scope();
        let iter_reg = self.cur().alloc_reg()?;
        let val_reg = self.cur().alloc_reg()?;
        let done_reg = self.cur().alloc_reg()?;
        self.compile_expr(right, iter_reg)?;
        let iter_op = match kind {
            ForEachKind::In => Opcode::ForInKeys,
            ForEachKind::Of => Opcode::GetIterator,
            ForEachKind::AsyncOf => Opcode::GetAsyncIterator,
        };
        self.cur()
            .emit(Instruction::abc(iter_op, iter_reg, iter_reg, 0));

        let mut per_iter_close_reg: Option<u8> = None;
        if let ForEachLeft::VarDecl {
            kind: var_kind, id, ..
        } = left
        {
            if *var_kind != VarKind::Var {
                let start_reg = self.cur().next_reg;
                self.declare_pattern_locals(id, *var_kind)?;
                per_iter_close_reg = Some(start_reg);
            }
        }

        let loop_start = self.cur().code.len();
        self.cur().loops.push(LoopControl {
            label,
            continue_target: if per_iter_close_reg.is_some() {
                None
            } else {
                Some(loop_start)
            },
            continue_jumps: Vec::new(),
            break_jumps: Vec::new(),
            is_switch: false,
        });

        if kind == ForEachKind::AsyncOf {
            let res_reg = self.cur().alloc_reg()?;
            self.cur().emit(Instruction::abc(
                Opcode::AsyncIteratorNext,
                res_reg,
                iter_reg,
                0,
            ));
            self.cur()
                .emit(Instruction::abc(Opcode::Await, res_reg, res_reg, 0));
            self.cur().emit(Instruction::abc(
                Opcode::UnpackIterResult,
                val_reg,
                res_reg,
                done_reg,
            ));
            self.cur().free_to(res_reg);
        } else {
            self.cur().emit(Instruction::abc(
                Opcode::IteratorNext,
                val_reg,
                iter_reg,
                done_reg,
            ));
        }
        let jmp_done = self.cur().emit_jump(Opcode::JumpIfTrue, done_reg);
        let body_try_start = self.cur().code.len() as u32;

        match left {
            ForEachLeft::VarDecl { id, .. } => {
                self.compile_pattern_bind(id, val_reg)?;
            }
            ForEachLeft::Target(target) => {
                self.compile_assign_target_store(target, val_reg)?;
            }
        }

        if kind != ForEachKind::In {
            self.cur()
                .active_iter_closes
                .push((iter_reg, kind == ForEachKind::AsyncOf));
        }
        self.compile_stmt(body)?;
        if kind != ForEachKind::In {
            self.cur().active_iter_closes.pop();
        }
        let continue_pc = self.cur().code.len();
        if let Some(close_reg) = per_iter_close_reg {
            self.cur()
                .emit(Instruction::abc(Opcode::CloseUpvalues, close_reg, 0, 0));
        }
        let body_try_end = self.cur().code.len() as u32;
        self.cur().emit_loop_back(loop_start)?;

        let end_pc = self.cur().code.len();
        let ctrl = self.cur().loops.pop().expect("loop frame");
        for j in ctrl.continue_jumps {
            self.cur().patch_jump(j, continue_pc)?;
        }
        if kind == ForEachKind::AsyncOf {
            // Normal completion (`done === true`) skips AsyncIteratorClose;
            // `break` and abrupt throws invoke `iter.return()` and await its result.
            for j in ctrl.break_jumps {
                self.cur().patch_jump(j, end_pc)?;
            }
            self.cur()
                .emit(Instruction::abc(Opcode::IteratorClose, iter_reg, 0, 1));
            self.cur()
                .emit(Instruction::abc(Opcode::Await, iter_reg, iter_reg, 0));
            let jmp_after_abrupt = self.cur().emit_jump(Opcode::Jump, 0);
            let handler_pc = self.cur().code.len() as u32;
            let exc_reg = self.cur().alloc_reg()?;
            self.cur().exception_handlers.push(ExceptionHandler {
                try_start: body_try_start,
                try_end: body_try_end,
                handler_pc,
                catch_reg: exc_reg,
                is_finally: true,
            });
            self.cur()
                .emit(Instruction::abc(Opcode::IteratorClose, iter_reg, 1, 1));
            let close_try_start = self.cur().code.len() as u32;
            self.cur()
                .emit(Instruction::abc(Opcode::Await, iter_reg, iter_reg, 0));
            let close_try_end = self.cur().code.len() as u32;
            let jmp_rethrow = self.cur().emit_jump(Opcode::Jump, 0);
            let swallow_pc = self.cur().code.len() as u32;
            let ignored_reg = self.cur().alloc_reg()?;
            self.cur().exception_handlers.push(ExceptionHandler {
                try_start: close_try_start,
                try_end: close_try_end,
                handler_pc: swallow_pc,
                catch_reg: ignored_reg,
                is_finally: false,
            });
            let rethrow_pc = self.cur().code.len();
            self.cur().patch_jump(jmp_rethrow, rethrow_pc)?;
            self.cur()
                .emit(Instruction::abc(Opcode::Throw, exc_reg, 0, 0));
            self.cur().free_to(exc_reg);
            let after_abrupt = self.cur().code.len();
            self.cur().patch_jump(jmp_done, after_abrupt)?;
            self.cur().patch_jump(jmp_after_abrupt, after_abrupt)?;
        } else {
            self.cur().patch_jump(jmp_done, end_pc)?;
            for j in ctrl.break_jumps {
                self.cur().patch_jump(j, end_pc)?;
            }
            if kind != ForEachKind::In {
                self.cur()
                    .emit(Instruction::abc(Opcode::IteratorClose, iter_reg, 0, 0));
                let jmp_after_abrupt = self.cur().emit_jump(Opcode::Jump, 0);
                let handler_pc = self.cur().code.len() as u32;
                let exc_reg = self.cur().alloc_reg()?;
                self.cur().exception_handlers.push(ExceptionHandler {
                    try_start: body_try_start,
                    try_end: body_try_end,
                    handler_pc,
                    catch_reg: exc_reg,
                    is_finally: true,
                });
                self.cur()
                    .emit(Instruction::abc(Opcode::IteratorClose, iter_reg, 1, 0));
                self.cur()
                    .emit(Instruction::abc(Opcode::Throw, exc_reg, 0, 0));
                self.cur().free_to(exc_reg);
                let after_abrupt = self.cur().code.len();
                self.cur().patch_jump(jmp_after_abrupt, after_abrupt)?;
            }
        }
        self.cur().end_scope();
        Ok(())
    }

    fn compile_var_decl(
        &mut self,
        kind: VarKind,
        decls: &[VarDeclarator],
    ) -> Result<(), CompileError> {
        let is_lex_global = self.frames.len() == 1
            && self.source_type == SourceType::Script
            && self.eval_mode == EvalScopeMode::None
            && self.cur().scope_depth == 0;

        if kind != VarKind::Var && !is_lex_global {
            for d in decls {
                self.declare_pattern_locals(&d.id, kind)?;
            }
        }

        for d in decls {
            let saved = self.cur().next_reg;
            let val_reg = self.cur().alloc_reg()?;
            let inferred = match &d.id {
                Pattern::Ident { name, .. } => Some(name.as_str()),
                _ => None,
            };
            if let Some(ref init) = d.init {
                self.compile_expr_named(init, val_reg, inferred)?;
            } else {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadUndef, val_reg, 0, 0));
            }
            self.compile_pattern_bind(&d.id, val_reg)?;
            self.cur().free_to(saved);
        }
        Ok(())
    }

    fn declare_pattern_locals(&mut self, pat: &Pattern, kind: VarKind) -> Result<(), CompileError> {
        let mut names = Vec::new();
        pat.collect_bound_names(&mut names);
        let is_const = kind == VarKind::Const;
        let cur_depth = self.cur().scope_depth;
        for (name, _) in names {
            let already = self
                .cur()
                .locals
                .iter()
                .any(|l| l.depth == cur_depth && l.name == name);
            if !already {
                let reg = self.cur().declare_local(&name, is_const, true)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadTdz, reg, 0, 0));
            }
        }
        Ok(())
    }

    fn compile_pattern_bind(&mut self, pat: &Pattern, val_reg: u8) -> Result<(), CompileError> {
        match pat {
            Pattern::Ident { name, .. } => {
                // Clear TDZ flag on local binding initialization and store value directly.
                let is_script_global = self.frames.len() == 1
                    && self.source_type == SourceType::Script
                    && self.eval_mode != EvalScopeMode::LocalAll
                    && self.cur().scope_depth == 0
                    && self.cur().find_local(name).is_none();
                if is_script_global {
                    let c = self.cur().add_string_const(name)?;
                    self.cur().emit_abx_wide(Opcode::DeclareGlobal, val_reg, c);
                } else if let Some((idx, l)) = self.cur().find_local(name) {
                    let reg = l.reg;
                    self.cur().locals[idx].is_tdz = false;
                    if reg != val_reg {
                        self.cur()
                            .emit(Instruction::abc(Opcode::Move, reg, val_reg, 0));
                    }
                } else {
                    self.emit_store_var(name, val_reg)?;
                }
            }
            Pattern::Assign { left, right, .. } => {
                let saved = self.cur().next_reg;
                let tmp = self.cur().alloc_reg()?;
                self.cur()
                    .emit(Instruction::abc(Opcode::Move, tmp, val_reg, 0));
                let jmp_skip = self.cur().emit_jump(Opcode::JumpIfNotUndef, tmp);
                let inferred = match left.as_ref() {
                    Pattern::Ident { name, .. } => Some(name.as_str()),
                    _ => None,
                };
                self.compile_expr_named(right, tmp, inferred)?;
                let after = self.cur().code.len();
                self.cur().patch_jump(jmp_skip, after)?;
                self.compile_pattern_bind(left, tmp)?;
                self.cur().free_to(saved);
            }
            Pattern::Array { elements, rest, .. } => {
                let saved = self.cur().next_reg;
                let iter_reg = self.cur().alloc_reg()?;
                let item_reg = self.cur().alloc_reg()?;
                let done_reg = self.cur().alloc_reg()?;
                self.cur()
                    .emit(Instruction::abc(Opcode::GetIterator, iter_reg, val_reg, 0));
                let try_start = self.cur().code.len() as u32;
                for elem in elements {
                    self.cur().emit(Instruction::abc(
                        Opcode::IteratorNext,
                        item_reg,
                        iter_reg,
                        done_reg,
                    ));
                    if let Some(sub_pat) = elem {
                        self.compile_pattern_bind(sub_pat, item_reg)?;
                    }
                }
                if let Some(rest_pat) = rest {
                    let rest_arr = self.cur().alloc_reg()?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::NewArray, rest_arr, 0, 0));
                    let loop_pc = self.cur().code.len();
                    self.cur().emit(Instruction::abc(
                        Opcode::IteratorNext,
                        item_reg,
                        iter_reg,
                        done_reg,
                    ));
                    let jmp_done = self.cur().emit_jump(Opcode::JumpIfTrue, done_reg);
                    self.cur()
                        .emit(Instruction::abc(Opcode::ArrayPush, rest_arr, item_reg, 0));
                    self.cur().emit_loop_back(loop_pc)?;
                    let end_pc = self.cur().code.len();
                    self.cur().patch_jump(jmp_done, end_pc)?;
                    self.compile_pattern_bind(rest_pat, rest_arr)?;
                }
                let try_end = self.cur().code.len() as u32;
                self.cur()
                    .emit(Instruction::abc(Opcode::IteratorClose, iter_reg, 0, 0));
                let jmp_after = self.cur().emit_jump(Opcode::Jump, 0);
                let handler_pc = self.cur().code.len() as u32;
                let exc_reg = self.cur().alloc_reg()?;
                self.cur().exception_handlers.push(ExceptionHandler {
                    try_start,
                    try_end,
                    handler_pc,
                    catch_reg: exc_reg,
                    is_finally: true,
                });
                self.cur()
                    .emit(Instruction::abc(Opcode::IteratorClose, iter_reg, 1, 0));
                self.cur()
                    .emit(Instruction::abc(Opcode::Throw, exc_reg, 0, 0));
                let after = self.cur().code.len();
                self.cur().patch_jump(jmp_after, after)?;
                self.cur().free_to(saved);
            }
            Pattern::Object { props, rest, .. } => {
                let saved = self.cur().next_reg;
                self.cur().emit(Instruction::abc(
                    Opcode::RequireObjectCoercible,
                    val_reg,
                    0,
                    0,
                ));
                let prop_val = self.cur().alloc_reg()?;
                let excluded_arr = if rest.is_some() {
                    let arr = self.cur().alloc_reg()?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::NewArray, arr, 0, 0));
                    Some(arr)
                } else {
                    None
                };

                for prop in props {
                    match &prop.key {
                        PropKey::Ident(name) | PropKey::String(name) => {
                            let c = self.cur().add_string_const(name)?;
                            let ic = self.cur().add_ic_slot(IcKind::GetProp, c, 0)?;
                            self.cur()
                                .emit_ic_abc(Opcode::GetProp, prop_val, val_reg, ic);
                            if let Some(excl) = excluded_arr {
                                let k = self.cur().alloc_reg()?;
                                self.cur().emit_abx_wide(Opcode::LoadConst, k, c);
                                self.cur()
                                    .emit(Instruction::abc(Opcode::ArrayPush, excl, k, 0));
                                self.cur().free_to(k);
                            }
                        }
                        PropKey::Number(n) => {
                            let c = self.cur().add_string_const(&n.to_string())?;
                            let ic = self.cur().add_ic_slot(IcKind::GetProp, c, 0)?;
                            self.cur()
                                .emit_ic_abc(Opcode::GetProp, prop_val, val_reg, ic);
                            if let Some(excl) = excluded_arr {
                                let k = self.cur().alloc_reg()?;
                                self.cur().emit_abx_wide(Opcode::LoadConst, k, c);
                                self.cur()
                                    .emit(Instruction::abc(Opcode::ArrayPush, excl, k, 0));
                                self.cur().free_to(k);
                            }
                        }
                        PropKey::Computed(key_expr) => {
                            let k = self.cur().alloc_reg()?;
                            self.compile_expr(key_expr, k)?;
                            self.cur().emit(Instruction::abc(
                                Opcode::GetElem,
                                prop_val,
                                val_reg,
                                k,
                            ));
                            if let Some(excl) = excluded_arr {
                                self.cur()
                                    .emit(Instruction::abc(Opcode::ArrayPush, excl, k, 0));
                            }
                            self.cur().free_to(k);
                        }
                        PropKey::Private(name) => {
                            let c = self.cur().add_string_const(name)?;
                            let ic = self.cur().add_ic_slot(IcKind::GetProp, c, 0)?;
                            self.cur()
                                .emit_ic_abc(Opcode::GetPrivate, prop_val, val_reg, ic);
                        }
                    }
                    self.compile_pattern_bind(&prop.value, prop_val)?;
                }

                if let (Some(rest_pat), Some(excl)) = (rest, excluded_arr) {
                    let rest_obj = self.cur().alloc_reg()?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::NewObject, rest_obj, 0, 0));
                    self.cur().emit(Instruction::abc(
                        Opcode::ObjectCopyProps,
                        rest_obj,
                        val_reg,
                        excl,
                    ));
                    self.compile_pattern_bind(rest_pat, rest_obj)?;
                }
                self.cur().free_to(saved);
            }
        }
        Ok(())
    }

    fn compile_assign_target_store(
        &mut self,
        target: &AssignTarget,
        val_reg: u8,
    ) -> Result<(), CompileError> {
        match target {
            AssignTarget::Ident { name, .. } => self.emit_store_var(name, val_reg),
            AssignTarget::Member {
                object, property, ..
            } => {
                let saved = self.cur().next_reg;
                let obj_reg = self.cur().alloc_reg()?;
                self.compile_expr(object, obj_reg)?;
                self.emit_member_store(obj_reg, property, val_reg)?;
                self.cur().free_to(saved);
                Ok(())
            }
            AssignTarget::Pattern(pat) => self.compile_pattern_bind(pat, val_reg),
        }
    }

    // -----------------------------------------------------------------------
    // Functions & Classes Compilation
    // -----------------------------------------------------------------------

    fn compile_function_body(
        &mut self,
        func: &Function,
        is_arrow: bool,
    ) -> Result<u32, CompileError> {
        self.compile_function_body_ex(func, is_arrow, false, None)
    }

    fn compile_function_body_ex(
        &mut self,
        func: &Function,
        is_arrow: bool,
        is_method: bool,
        inferred_name: Option<&str>,
    ) -> Result<u32, CompileError> {
        let resolved_name = func
            .name
            .clone()
            .or_else(|| inferred_name.map(ToOwned::to_owned));
        let expected_len = func
            .params
            .iter()
            .take_while(|p| !matches!(p, Pattern::Assign { .. }))
            .count() as u16;
        let parent_in_class_field = is_arrow
            && self
                .frames
                .last()
                .is_some_and(|f| f.in_class_field_or_static);
        let parent_in_class_body = self.frames.last().is_some_and(|f| f.in_class_body);
        let mut frame = FunctionCompiler::new(
            resolved_name,
            func.params.len() as u16,
            func.strict,
            func.is_async,
            func.is_generator,
            is_arrow,
        );
        frame.length = expected_len;
        frame.is_method = is_method;
        frame.in_class_field_or_static = parent_in_class_field;
        frame.in_class_body = parent_in_class_body;
        self.frames.push(frame);

        // Bind positional parameter registers (`0..params.len()`).
        let has_complex_params = func
            .params
            .iter()
            .any(|p| !matches!(p, Pattern::Ident { .. }))
            || func.rest_param.is_some();
        let mut param_binds_arguments = false;
        let mut all_param_names = Vec::new();
        for pat in &func.params {
            let mut bound = Vec::new();
            pat.collect_bound_names(&mut bound);
            for (n, _) in bound {
                if n == "arguments" {
                    param_binds_arguments = true;
                }
                if !all_param_names.contains(&n) {
                    all_param_names.push(n);
                }
            }
        }
        if let Some(ref rest_pat) = func.rest_param {
            let mut bound = Vec::new();
            rest_pat.collect_bound_names(&mut bound);
            for (n, _) in bound {
                if n == "arguments" {
                    param_binds_arguments = true;
                }
                if !all_param_names.contains(&n) {
                    all_param_names.push(n);
                }
            }
        }
        self.cur().param_names = all_param_names;
        if !has_complex_params {
            for pat in &func.params {
                if let Pattern::Ident { name, .. } = pat {
                    let _ = self.cur().declare_local(name, false, false)?;
                }
            }
            if !is_arrow && !param_binds_arguments && function_uses_arguments(func) {
                let args_reg = self.cur().declare_local("arguments", false, false)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadArguments, args_reg, 0, 0));
            }
        } else {
            // Reserve incoming argument registers `0..params.len()`, materialize `arguments`
            // if referenced, pre-declare all parameter bindings in TDZ, then bind left-to-right.
            let mut raw_regs = Vec::with_capacity(func.params.len());
            for _ in &func.params {
                raw_regs.push(self.cur().alloc_reg()?);
            }
            if !is_arrow && !param_binds_arguments && function_uses_arguments(func) {
                let args_reg = self.cur().declare_local("arguments", false, false)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadArguments, args_reg, 0, 0));
            }
            for pat in &func.params {
                self.declare_pattern_locals(pat, VarKind::Let)?;
            }
            if let Some(ref rest_pat) = func.rest_param {
                self.declare_pattern_locals(rest_pat, VarKind::Let)?;
            }
            self.cur().in_param_init = true;
            for (raw_reg, pat) in raw_regs.into_iter().zip(&func.params) {
                self.compile_pattern_bind(pat, raw_reg)?;
            }
            if let Some(ref rest_pat) = func.rest_param {
                let rest_reg = self.cur().alloc_reg()?;
                let first_rest = func.params.len().min(255) as u8;
                self.cur().emit(Instruction::abc(
                    Opcode::CollectRestArgs,
                    rest_reg,
                    first_rest,
                    0,
                ));
                self.compile_pattern_bind(rest_pat, rest_reg)?;
            }
            self.cur().in_param_init = false;
            // Parameter bindings are not body lexical declarations for later body `var`s.
            for l in &mut self.cur().locals {
                l.is_lex = false;
            }
        }

        // Generators & async generators suspend here (after synchronous parameter initialization).
        if func.is_generator {
            self.cur()
                .emit(Instruction::abc(Opcode::GeneratorStart, 0, 0, 0));
        }

        self.hoist_declarations(&func.body, false)?;
        for stmt in &func.body {
            self.compile_stmt(stmt)?;
        }

        let r = self.cur().alloc_reg()?;
        self.cur()
            .emit(Instruction::abc(Opcode::LoadUndef, r, 0, 0));
        self.cur().emit(Instruction::abc(Opcode::Return, r, 0, 0));

        let compiled = self.frames.pop().expect("func frame").finish();
        let idx = self.functions.len() as u32;
        self.functions.push(compiled);
        Ok(idx)
    }

    fn compile_class(&mut self, cls: &Class, dst: u8) -> Result<(), CompileError> {
        self.compile_class_named(cls, dst, None)
    }

    fn compile_class_named(
        &mut self,
        cls: &Class,
        dst: u8,
        inferred_name: Option<&str>,
    ) -> Result<(), CompileError> {
        let saved = self.cur().next_reg;
        let super_reg = self.cur().alloc_reg()?;
        if let Some(ref sup) = cls.super_class {
            self.compile_expr(sup, super_reg)?;
        } else {
            self.cur()
                .emit(Instruction::abc(Opcode::LoadUndef, super_reg, 0, 0));
        }

        let mut constructor_idx = None;
        let mut methods = Vec::new();
        let mut computed_methods: Vec<(&Expr, MethodKind, bool, u32)> = Vec::new();
        let mut instance_fields = Vec::new();
        let mut static_inits = Vec::new();

        for elem in &cls.elements {
            match elem {
                ClassElement::Method {
                    key,
                    kind,
                    is_static,
                    function,
                    ..
                } => {
                    if *kind == MethodKind::Constructor && !is_static {
                        let fn_idx = self.compile_function_body_ex(
                            function,
                            false,
                            false,
                            cls.name.as_deref().or(inferred_name),
                        )?;
                        constructor_idx = Some(fn_idx);
                    } else if let PropKey::Computed(key_expr) = key {
                        let fn_idx = self.compile_function_body_ex(function, false, true, None)?;
                        computed_methods.push((key_expr, *kind, *is_static, fn_idx));
                    } else {
                        let (name, is_private) = match key {
                            PropKey::Ident(n) | PropKey::String(n) => (n.clone(), false),
                            PropKey::Private(n) => (n.clone(), true),
                            PropKey::Number(n) => (n.to_string(), false),
                            PropKey::Computed(_) => unreachable!(),
                        };
                        let disp_name = match kind {
                            MethodKind::Get => {
                                if is_private {
                                    format!("get #{name}")
                                } else {
                                    format!("get {name}")
                                }
                            }
                            MethodKind::Set => {
                                if is_private {
                                    format!("set #{name}")
                                } else {
                                    format!("set {name}")
                                }
                            }
                            _ => {
                                if is_private {
                                    format!("#{name}")
                                } else {
                                    name.clone()
                                }
                            }
                        };
                        let fn_idx =
                            self.compile_function_body_ex(function, false, true, Some(&disp_name))?;
                        methods.push(ClassMethodBlueprint {
                            name,
                            is_private,
                            is_static: *is_static,
                            kind: (*kind).into(),
                            func_idx: fn_idx,
                        });
                    }
                }
                ClassElement::Field {
                    key,
                    is_static,
                    value,
                    ..
                } => {
                    if *is_static {
                        static_inits.push(StaticInitItem::Field(key, value.as_ref()));
                    } else {
                        instance_fields.push((key, value.as_ref()));
                    }
                }
                ClassElement::StaticBlock { body, .. } => {
                    static_inits.push(StaticInitItem::Block(body));
                }
            }
        }

        let instance_field_init_idx = if !instance_fields.is_empty() {
            Some(self.compile_field_initializer(&instance_fields)?)
        } else {
            None
        };
        let static_init_idx = if !static_inits.is_empty() {
            Some(self.compile_static_initializer(&static_inits)?)
        } else {
            None
        };

        let defer_static_init = !computed_methods.is_empty() && static_init_idx.is_some();
        let blueprint = ClassBlueprint {
            name: cls
                .name
                .clone()
                .or_else(|| inferred_name.map(ToOwned::to_owned)),
            has_super: cls.super_class.is_some(),
            constructor_idx,
            methods,
            instance_field_init_idx,
            static_init_idx: if defer_static_init {
                None
            } else {
                static_init_idx
            },
        };
        let c = self.cur().add_const(ConstantEntry::Class(blueprint))?;
        self.cur()
            .emit_ic_abc(Opcode::CreateClass, dst, super_reg, c);

        if !computed_methods.is_empty() {
            let proto_reg = self.cur().alloc_reg()?;
            let c_proto = self.cur().add_string_const("prototype")?;
            let ic_proto = self.cur().add_ic_slot(IcKind::GetProp, c_proto, 0)?;
            self.cur()
                .emit_ic_abc(Opcode::GetProp, proto_reg, dst, ic_proto);
            let key_reg = self.cur().alloc_reg()?;
            let fn_reg = self.cur().alloc_reg()?;
            for (key_expr, kind, is_static, fn_idx) in computed_methods {
                self.compile_expr(key_expr, key_reg)?;
                let fc = self.cur().add_const(ConstantEntry::Function(fn_idx))?;
                self.cur().emit_abx_wide(Opcode::NewClosure, fn_reg, fc);
                let target_reg = if is_static { dst } else { proto_reg };
                let def_op = match kind {
                    MethodKind::Get => Opcode::DefineClassGetter,
                    MethodKind::Set => Opcode::DefineClassSetter,
                    _ => Opcode::DefineClassMethod,
                };
                self.cur()
                    .emit(Instruction::abc(def_op, target_reg, key_reg, fn_reg));
            }
            if let Some(si_idx) = static_init_idx.filter(|_| defer_static_init) {
                let call_base = self.cur().alloc_reg()?;
                let this_slot = self.cur().alloc_reg()?;
                let fc = self.cur().add_const(ConstantEntry::Function(si_idx))?;
                self.cur().emit_abx_wide(Opcode::NewClosure, call_base, fc);
                self.cur()
                    .emit(Instruction::abc(Opcode::Move, this_slot, dst, 0));
                self.cur().emit(Instruction::abc(
                    Opcode::CallWithThis,
                    key_reg,
                    call_base,
                    0,
                ));
            }
        }
        self.cur().free_to(saved);
        Ok(())
    }

    fn compile_field_initializer(
        &mut self,
        fields: &[(&PropKey, Option<&Expr>)],
    ) -> Result<u32, CompileError> {
        let mut frame =
            FunctionCompiler::new(Some("<field_init>".into()), 0, true, false, false, false);
        frame.in_class_field_or_static = true;
        frame.in_class_body = true;
        self.frames.push(frame);
        let this_reg = self.cur().alloc_reg()?;
        let val_reg = self.cur().alloc_reg()?;
        self.cur()
            .emit(Instruction::abc(Opcode::LoadThis, this_reg, 0, 0));
        for (key, value) in fields {
            if let Some(expr) = value {
                self.compile_expr(expr, val_reg)?;
            } else {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadUndef, val_reg, 0, 0));
            }
            self.emit_prop_define(this_reg, key, val_reg)?;
        }
        self.cur()
            .emit(Instruction::abc(Opcode::LoadUndef, val_reg, 0, 0));
        self.cur()
            .emit(Instruction::abc(Opcode::Return, val_reg, 0, 0));
        let compiled = self.frames.pop().expect("field init frame").finish();
        let idx = self.functions.len() as u32;
        self.functions.push(compiled);
        Ok(idx)
    }

    fn compile_static_initializer(
        &mut self,
        items: &[StaticInitItem<'_>],
    ) -> Result<u32, CompileError> {
        let mut frame =
            FunctionCompiler::new(Some("<static_init>".into()), 0, true, false, false, false);
        frame.in_class_field_or_static = true;
        frame.in_class_body = true;
        self.frames.push(frame);
        let this_reg = self.cur().alloc_reg()?;
        let val_reg = self.cur().alloc_reg()?;
        self.cur()
            .emit(Instruction::abc(Opcode::LoadThis, this_reg, 0, 0));
        for item in items {
            match item {
                StaticInitItem::Field(key, value) => {
                    if let Some(expr) = value {
                        self.compile_expr(expr, val_reg)?;
                    } else {
                        self.cur()
                            .emit(Instruction::abc(Opcode::LoadUndef, val_reg, 0, 0));
                    }
                    self.emit_prop_define(this_reg, key, val_reg)?;
                }
                StaticInitItem::Block(stmts) => {
                    self.cur().begin_scope();
                    self.hoist_block_declarations(stmts)?;
                    for s in *stmts {
                        self.compile_stmt(s)?;
                    }
                    self.cur().end_scope();
                }
            }
        }
        self.cur()
            .emit(Instruction::abc(Opcode::LoadUndef, val_reg, 0, 0));
        self.cur()
            .emit(Instruction::abc(Opcode::Return, val_reg, 0, 0));
        let compiled = self.frames.pop().expect("static init frame").finish();
        let idx = self.functions.len() as u32;
        self.functions.push(compiled);
        Ok(idx)
    }

    // -----------------------------------------------------------------------
    // Expression Compilation
    // -----------------------------------------------------------------------

    fn compile_expr(&mut self, expr: &Expr, dst: u8) -> Result<(), CompileError> {
        self.compile_expr_named(expr, dst, None)
    }

    fn compile_expr_named(
        &mut self,
        expr: &Expr,
        dst: u8,
        inferred_name: Option<&str>,
    ) -> Result<(), CompileError> {
        match &expr.kind {
            ExprKind::Paren(inner) => self.compile_expr_named(inner, dst, inferred_name),
            ExprKind::Null => {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadNull, dst, 0, 0));
                Ok(())
            }
            ExprKind::Bool(true) => {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadTrue, dst, 0, 0));
                Ok(())
            }
            ExprKind::Bool(false) => {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadFalse, dst, 0, 0));
                Ok(())
            }
            ExprKind::Number(n) => self.emit_number(*n, dst),
            ExprKind::BigInt(raw) => {
                let c = self.cur().add_const(ConstantEntry::BigInt(raw.clone()))?;
                self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
                Ok(())
            }
            ExprKind::String(s) => {
                let c = self.cur().add_string_const(s)?;
                self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
                Ok(())
            }
            ExprKind::RegExp { pattern, flags } => {
                let c = self.cur().add_const(ConstantEntry::RegExp {
                    pattern: pattern.clone(),
                    flags: flags.clone(),
                })?;
                self.cur().emit_abx_wide(Opcode::NewRegExp, dst, c);
                Ok(())
            }
            ExprKind::Ident(name) => self.emit_load_var(name, dst),
            ExprKind::PrivateIdent(name) => {
                let c = self.cur().add_string_const(name)?;
                self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
                Ok(())
            }
            ExprKind::This => {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadThis, dst, 0, 0));
                Ok(())
            }
            ExprKind::Super => {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadThis, dst, 0, 0));
                Ok(())
            }
            ExprKind::NewTarget => {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadNewTarget, dst, 0, 0));
                Ok(())
            }
            ExprKind::ImportMeta => {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadImportMeta, dst, 0, 0));
                Ok(())
            }
            ExprKind::Array(elements) => {
                let has_spread_or_hole = elements
                    .iter()
                    .any(|e| matches!(e, None | Some(ArrayElement::Spread(_))));
                if !has_spread_or_hole && elements.len() <= 32 {
                    let saved = self.cur().next_reg;
                    let base = self.cur().alloc_regs(elements.len() as u8)?;
                    for (i, el) in elements.iter().enumerate() {
                        if let Some(ArrayElement::Expr(e)) = el {
                            self.compile_expr(e, base + i as u8)?;
                        }
                    }
                    self.cur().emit(Instruction::abc(
                        Opcode::NewArray,
                        dst,
                        base,
                        elements.len() as u8,
                    ));
                    self.cur().free_to(saved);
                } else {
                    self.cur()
                        .emit(Instruction::abc(Opcode::NewArray, dst, 0, 0));
                    let saved = self.cur().next_reg;
                    let tmp = self.cur().alloc_reg()?;
                    for el in elements {
                        match el {
                            None => {
                                self.cur()
                                    .emit(Instruction::abc(Opcode::ArrayPushHole, dst, 0, 0));
                            }
                            Some(ArrayElement::Expr(e)) => {
                                self.compile_expr(e, tmp)?;
                                self.cur()
                                    .emit(Instruction::abc(Opcode::ArrayPush, dst, tmp, 0));
                            }
                            Some(ArrayElement::Spread(e)) => {
                                self.compile_expr(e, tmp)?;
                                self.cur().emit(Instruction::abc(
                                    Opcode::ArrayAppendIterable,
                                    dst,
                                    tmp,
                                    0,
                                ));
                            }
                        }
                    }
                    self.cur().free_to(saved);
                }
                Ok(())
            }
            ExprKind::Object(props) => {
                // Early Error checks (ECMA-262 §13.2.5.1):
                // 1) Unconverted CoverInitializedName (`{ a = 1 }`) outside destructuring.
                // 2) Duplicate non-computed `__proto__: expr` data properties.
                let mut proto_data_count = 0u32;
                for prop in props {
                    if let ObjectProp::KeyValue {
                        key,
                        value,
                        shorthand,
                        span,
                    } = prop
                    {
                        if *shorthand && matches!(value.kind, ExprKind::Assign { .. }) {
                            return Err(CompileError::Syntax(SyntaxError::new(
                                "Invalid shorthand property initializer",
                                *span,
                            )));
                        }
                        if !*shorthand {
                            if let PropKey::Ident(n) | PropKey::String(n) = key {
                                if n == "__proto__" {
                                    proto_data_count += 1;
                                    if proto_data_count > 1 {
                                        return Err(CompileError::Syntax(SyntaxError::new(
                                            "Duplicate __proto__ fields are not allowed in object literals",
                                            *span,
                                        )));
                                    }
                                }
                            }
                        }
                    }
                }
                self.cur().emit(Instruction::abc(
                    Opcode::NewObject,
                    dst,
                    props.len().min(255) as u8,
                    0,
                ));
                let saved = self.cur().next_reg;
                let val_reg = self.cur().alloc_reg()?;
                for prop in props {
                    match prop {
                        ObjectProp::KeyValue { key, value, .. } => {
                            if matches!(key, PropKey::Ident(n) | PropKey::String(n) if n == "__proto__")
                            {
                                self.compile_expr(value, val_reg)?;
                                self.cur().emit(Instruction::abc(
                                    Opcode::SetProto,
                                    dst,
                                    val_reg,
                                    0,
                                ));
                            } else {
                                let prop_name = match key {
                                    PropKey::Ident(n) | PropKey::String(n) => Some(n.as_str()),
                                    _ => None,
                                };
                                self.compile_expr_named(value, val_reg, prop_name)?;
                                self.emit_prop_define(dst, key, val_reg)?;
                            }
                        }
                        ObjectProp::Method {
                            key,
                            kind,
                            function,
                            ..
                        } => {
                            let base_name = match key {
                                PropKey::Ident(n) | PropKey::String(n) => Some(n.clone()),
                                PropKey::Number(n) => Some(n.to_string()),
                                _ => None,
                            };
                            let m_name = base_name.map(|n| match kind {
                                MethodKind::Get => format!("get {n}"),
                                MethodKind::Set => format!("set {n}"),
                                _ => n,
                            });
                            let fn_idx = self.compile_function_body_ex(
                                function,
                                false,
                                true,
                                m_name.as_deref(),
                            )?;
                            let c = self.cur().add_const(ConstantEntry::Function(fn_idx))?;
                            self.cur().emit_abx_wide(Opcode::NewClosure, val_reg, c);
                            match kind {
                                MethodKind::Get | MethodKind::Set => {
                                    let key_reg = self.cur().alloc_reg()?;
                                    self.compile_prop_key_into_reg(key, key_reg)?;
                                    let op = if *kind == MethodKind::Get {
                                        Opcode::DefineGetter
                                    } else {
                                        Opcode::DefineSetter
                                    };
                                    self.cur().emit(Instruction::abc(op, dst, key_reg, val_reg));
                                    self.cur().free_to(key_reg);
                                }
                                _ => self.emit_prop_define(dst, key, val_reg)?,
                            }
                        }
                        ObjectProp::Spread { arg, .. } => {
                            self.compile_expr(arg, val_reg)?;
                            let excl = self.cur().alloc_reg()?;
                            self.cur()
                                .emit(Instruction::abc(Opcode::LoadNull, excl, 0, 0));
                            self.cur().emit(Instruction::abc(
                                Opcode::ObjectCopyProps,
                                dst,
                                val_reg,
                                excl,
                            ));
                            self.cur().free_to(excl);
                        }
                    }
                }
                self.cur().free_to(saved);
                Ok(())
            }
            ExprKind::Template { quasis, exprs } => {
                let first = quasis
                    .first()
                    .and_then(|q| q.cooked.as_deref())
                    .unwrap_or("");
                let c = self.cur().add_string_const(first)?;
                self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
                let saved = self.cur().next_reg;
                let tmp = self.cur().alloc_reg()?;
                for (expr, quasi) in exprs.iter().zip(quasis.iter().skip(1)) {
                    self.compile_expr(expr, tmp)?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::ToString, tmp, tmp, 0));
                    self.cur()
                        .emit(Instruction::abc(Opcode::Add, dst, dst, tmp));
                    let cooked = quasi.cooked.as_deref().unwrap_or("");
                    if !cooked.is_empty() {
                        let qc = self.cur().add_string_const(cooked)?;
                        self.cur().emit_abx_wide(Opcode::LoadConst, tmp, qc);
                        self.cur()
                            .emit(Instruction::abc(Opcode::Add, dst, dst, tmp));
                    }
                }
                self.cur().free_to(saved);
                Ok(())
            }
            ExprKind::TaggedTemplate { tag, quasis, exprs } => {
                let saved = self.cur().next_reg;
                let argc = 1 + exprs.len();
                let base = self.cur().alloc_regs((1 + argc) as u8)?;
                self.compile_expr(tag, base)?;

                // Build template object array (`quasis` cooked strings + `.raw`).
                let tpl_reg = base + 1;
                self.cur()
                    .emit(Instruction::abc(Opcode::NewArray, tpl_reg, 0, 0));
                let raw_reg = self.cur().alloc_reg()?;
                self.cur()
                    .emit(Instruction::abc(Opcode::NewArray, raw_reg, 0, 0));
                let tmp = self.cur().alloc_reg()?;
                for q in quasis {
                    if let Some(ref cooked) = q.cooked {
                        let c = self.cur().add_string_const(cooked)?;
                        self.cur().emit_abx_wide(Opcode::LoadConst, tmp, c);
                    } else {
                        self.cur()
                            .emit(Instruction::abc(Opcode::LoadUndef, tmp, 0, 0));
                    }
                    self.cur()
                        .emit(Instruction::abc(Opcode::ArrayPush, tpl_reg, tmp, 0));
                    let rc = self.cur().add_string_const(&q.raw)?;
                    self.cur().emit_abx_wide(Opcode::LoadConst, tmp, rc);
                    self.cur()
                        .emit(Instruction::abc(Opcode::ArrayPush, raw_reg, tmp, 0));
                }
                let raw_key = self.cur().add_string_const("raw")?;
                let raw_ic = self.cur().add_ic_slot(IcKind::DefineProp, raw_key, 0)?;
                self.cur()
                    .emit_ic_abc(Opcode::DefineProp, tpl_reg, raw_reg, raw_ic);

                for (i, e) in exprs.iter().enumerate() {
                    self.compile_expr(e, base + 2 + i as u8)?;
                }
                self.cur()
                    .emit(Instruction::abc(Opcode::Call, dst, base, argc as u8));
                self.cur().free_to(saved);
                Ok(())
            }
            ExprKind::Function(func) => {
                if let Some(ref self_name) = func.name {
                    // ECMA-262 §15.2.5: Named function expressions introduce an immutable
                    // self-binding visible ONLY within the function body (not enclosing scope).
                    let saved_reg = self.cur().next_reg;
                    self.cur().scope_depth += 1;
                    let self_reg = self.cur().declare_local(self_name, true, false)?;
                    if let Some(last) = self.cur().locals.last_mut() {
                        last.is_fn_expr_name = true;
                    }
                    let fn_idx =
                        self.compile_function_body_ex(func, false, false, inferred_name)?;
                    let c = self.cur().add_const(ConstantEntry::Function(fn_idx))?;
                    self.cur().emit_abx_wide(Opcode::NewClosure, dst, c);
                    self.cur()
                        .emit(Instruction::abc(Opcode::Move, self_reg, dst, 0));
                    if self.cur().locals.last().is_some_and(|l| l.is_captured) {
                        self.cur()
                            .emit(Instruction::abc(Opcode::CloseUpvalues, self_reg, 0, 0));
                    }
                    self.cur().locals.pop();
                    self.cur().scope_depth -= 1;
                    self.cur().free_to(saved_reg);
                    Ok(())
                } else {
                    let fn_idx =
                        self.compile_function_body_ex(func, false, false, inferred_name)?;
                    let c = self.cur().add_const(ConstantEntry::Function(fn_idx))?;
                    self.cur().emit_abx_wide(Opcode::NewClosure, dst, c);
                    Ok(())
                }
            }
            ExprKind::Arrow {
                params,
                rest_param,
                body,
                is_async,
                has_simple_params,
            } => {
                let stmts = match body {
                    ArrowBody::Block { body, .. } => body.clone(),
                    ArrowBody::Expr(e) => vec![Statement::Return {
                        arg: Some((**e).clone()),
                        span: e.span,
                    }],
                };
                let strict = match body {
                    ArrowBody::Block { strict, .. } => *strict,
                    ArrowBody::Expr(_) => self.cur().is_strict,
                };
                let synth = Function {
                    name: inferred_name.map(ToOwned::to_owned),
                    params: params.clone(),
                    rest_param: rest_param.clone(),
                    body: stmts,
                    is_async: *is_async,
                    is_generator: false,
                    strict,
                    has_simple_params: *has_simple_params,
                    span: expr.span,
                };
                let fn_idx = self.compile_function_body_ex(&synth, true, false, inferred_name)?;
                let c = self.cur().add_const(ConstantEntry::Function(fn_idx))?;
                self.cur().emit_abx_wide(Opcode::NewClosure, dst, c);
                Ok(())
            }
            ExprKind::Class(cls) => self.compile_class_named(cls, dst, inferred_name),
            ExprKind::Chain(inner) => {
                self.cur().chain_exits.push(Vec::new());
                self.compile_expr(inner, dst)?;
                let exits = self.cur().chain_exits.pop().expect("chain exits");
                if !exits.is_empty() {
                    let jmp_done = self.cur().emit_jump(Opcode::Jump, 0);
                    let nullish_pc = self.cur().code.len();
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, dst, 0, 0));
                    for (j, _) in exits {
                        self.cur().patch_jump(j, nullish_pc)?;
                    }
                    let end_pc = self.cur().code.len();
                    self.cur().patch_jump(jmp_done, end_pc)?;
                }
                Ok(())
            }
            ExprKind::Member {
                object,
                property,
                optional,
            } => {
                let saved = self.cur().next_reg;
                let obj_reg = self.cur().alloc_reg()?;
                if matches!(object.kind, ExprKind::Super) {
                    let key_reg = self.cur().alloc_reg()?;
                    self.compile_member_prop_key(property, key_reg)?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::GetSuperProp, dst, key_reg, 0));
                    self.cur().free_to(saved);
                    return Ok(());
                }
                self.compile_expr(object, obj_reg)?;
                if *optional {
                    let j = self.cur().emit_jump(Opcode::JumpIfNullish, obj_reg);
                    if let Some(exits) = self.cur().chain_exits.last_mut() {
                        exits.push((j, dst));
                    }
                }
                self.emit_member_load(dst, obj_reg, property)?;
                self.cur().free_to(saved);
                Ok(())
            }
            ExprKind::Call {
                callee,
                args,
                optional,
            } => self.compile_call(callee, args, *optional, dst),
            ExprKind::New { callee, args } => self.compile_new(callee, args, dst),
            ExprKind::ImportCall { source } => {
                let saved = self.cur().next_reg;
                let src_reg = self.cur().alloc_reg()?;
                self.compile_expr(source, src_reg)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::DynamicImport, dst, src_reg, 0));
                self.cur().free_to(saved);
                Ok(())
            }
            ExprKind::Unary { op, arg } => self.compile_unary(*op, arg, dst),
            ExprKind::Update { op, arg, prefix } => self.compile_update(*op, arg, *prefix, dst),
            ExprKind::Binary { op, left, right } => {
                if *op == BinaryOp::In {
                    if let ExprKind::PrivateIdent(ref priv_name) = left.kind {
                        let saved = self.cur().next_reg;
                        let obj_reg = self.cur().alloc_reg()?;
                        self.compile_expr(right, obj_reg)?;
                        let c = self.cur().add_string_const(priv_name)?;
                        let ic = self.cur().add_ic_slot(IcKind::GetProp, c, 0)?;
                        self.cur().emit_ic_abc(Opcode::InPrivate, dst, obj_reg, ic);
                        self.cur().free_to(saved);
                        return Ok(());
                    }
                }
                let saved = self.cur().next_reg;
                let lhs = self.cur().alloc_reg()?;
                let rhs = self.cur().alloc_reg()?;
                self.compile_expr(left, lhs)?;
                self.compile_expr(right, rhs)?;
                let opcode = match op {
                    BinaryOp::Add => Opcode::Add,
                    BinaryOp::Sub => Opcode::Sub,
                    BinaryOp::Mul => Opcode::Mul,
                    BinaryOp::Div => Opcode::Div,
                    BinaryOp::Rem => Opcode::Rem,
                    BinaryOp::Exp => Opcode::Exp,
                    BinaryOp::BitAnd => Opcode::BitAnd,
                    BinaryOp::BitOr => Opcode::BitOr,
                    BinaryOp::BitXor => Opcode::BitXor,
                    BinaryOp::Shl => Opcode::Shl,
                    BinaryOp::Shr => Opcode::Shr,
                    BinaryOp::UShr => Opcode::UShr,
                    BinaryOp::Eq => Opcode::Eq,
                    BinaryOp::NotEq => Opcode::NotEq,
                    BinaryOp::StrictEq => Opcode::StrictEq,
                    BinaryOp::StrictNotEq => Opcode::StrictNotEq,
                    BinaryOp::Lt => Opcode::Lt,
                    BinaryOp::LtEq => Opcode::LtEq,
                    BinaryOp::Gt => Opcode::Gt,
                    BinaryOp::GtEq => Opcode::GtEq,
                    BinaryOp::In => Opcode::In,
                    BinaryOp::InstanceOf => Opcode::InstanceOf,
                };
                self.cur().emit(Instruction::abc(opcode, dst, lhs, rhs));
                self.cur().free_to(saved);
                Ok(())
            }
            ExprKind::Logical { op, left, right } => {
                self.compile_expr(left, dst)?;
                let jmp = match op {
                    LogicalOp::And => self.cur().emit_jump(Opcode::JumpIfFalse, dst),
                    LogicalOp::Or => self.cur().emit_jump(Opcode::JumpIfTrue, dst),
                    LogicalOp::Nullish => {
                        // Short-circuit if NOT nullish: JumpIfNullish -> eval rhs, else skip rhs.
                        let j_eval_rhs = self.cur().emit_jump(Opcode::JumpIfNullish, dst);
                        let j_done = self.cur().emit_jump(Opcode::Jump, 0);
                        let rhs_pc = self.cur().code.len();
                        self.cur().patch_jump(j_eval_rhs, rhs_pc)?;
                        self.compile_expr(right, dst)?;
                        let end_pc = self.cur().code.len();
                        self.cur().patch_jump(j_done, end_pc)?;
                        return Ok(());
                    }
                };
                self.compile_expr(right, dst)?;
                let end_pc = self.cur().code.len();
                self.cur().patch_jump(jmp, end_pc)?;
                Ok(())
            }
            ExprKind::Conditional {
                test,
                consequent,
                alternate,
            } => {
                let saved = self.cur().next_reg;
                let cond = self.cur().alloc_reg()?;
                self.compile_expr(test, cond)?;
                let jmp_alt = self.cur().emit_jump(Opcode::JumpIfFalse, cond);
                self.cur().free_to(saved);

                self.compile_expr(consequent, dst)?;
                let jmp_end = self.cur().emit_jump(Opcode::Jump, 0);
                let alt_pc = self.cur().code.len();
                self.cur().patch_jump(jmp_alt, alt_pc)?;
                self.compile_expr(alternate, dst)?;
                let end_pc = self.cur().code.len();
                self.cur().patch_jump(jmp_end, end_pc)?;
                Ok(())
            }
            ExprKind::Assign { op, left, right } => self.compile_assign(*op, left, right, dst),
            ExprKind::Sequence(exprs) => {
                for e in exprs {
                    self.compile_expr(e, dst)?;
                }
                Ok(())
            }
            ExprKind::Await(arg) => {
                if self.frames.len() == 1 {
                    if let Some(ref mut m) = self.module {
                        m.has_top_level_await = true;
                    }
                }
                self.compile_expr(arg, dst)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::Await, dst, dst, 0));
                Ok(())
            }
            ExprKind::Yield { arg, delegate } => {
                if let Some(e) = arg {
                    self.compile_expr(e, dst)?;
                } else {
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadUndef, dst, 0, 0));
                }
                let op = if *delegate {
                    Opcode::YieldStar
                } else {
                    Opcode::Yield
                };
                self.cur().emit(Instruction::abc(op, dst, dst, 0));
                Ok(())
            }
        }
    }

    fn compile_call(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
        optional: bool,
        dst: u8,
    ) -> Result<(), CompileError> {
        let has_spread = args.iter().any(|a| matches!(a, CallArg::Spread(_)));
        let saved = self.cur().next_reg;

        // `super(args...)` constructor call.
        if matches!(callee.kind, ExprKind::Super) {
            if has_spread {
                let args_arr = self.cur().alloc_reg()?;
                self.compile_spread_args_array(args, args_arr)?;
                self.cur().emit(Instruction::abc(
                    Opcode::SuperConstruct,
                    dst,
                    args_arr,
                    u8::MAX,
                ));
                self.cur().free_to(saved);
                return Ok(());
            }
            let base = self.cur().alloc_regs((args.len() + 1) as u8)?;
            self.cur()
                .emit(Instruction::abc(Opcode::LoadThis, base, 0, 0));
            for (i, a) in args.iter().enumerate() {
                match a {
                    CallArg::Expr(e) | CallArg::Spread(e) => {
                        self.compile_expr(e, base + 1 + i as u8)?;
                    }
                }
            }
            self.cur().emit(Instruction::abc(
                Opcode::SuperConstruct,
                dst,
                base,
                args.len() as u8,
            ));
            self.cur().free_to(saved);
            return Ok(());
        }

        // Fast-path IC `CallMethod`: `obj.method(a, b)` (without spread, non-optional callee).
        if !has_spread && !optional {
            if let ExprKind::Member {
                object,
                property: MemberProp::Ident(prop_name),
                optional: mem_opt,
            } = &callee.kind
            {
                if !matches!(object.kind, ExprKind::Super) && args.len() <= 64 {
                    let base = self.cur().alloc_regs((1 + args.len()) as u8)?;
                    self.compile_expr(object, base)?;
                    if *mem_opt {
                        let j = self.cur().emit_jump(Opcode::JumpIfNullish, base);
                        if let Some(exits) = self.cur().chain_exits.last_mut() {
                            exits.push((j, dst));
                        }
                    }
                    for (i, a) in args.iter().enumerate() {
                        if let CallArg::Expr(e) = a {
                            self.compile_expr(e, base + 1 + i as u8)?;
                        }
                    }
                    let c = self.cur().add_string_const(prop_name)?;
                    let ic = self
                        .cur()
                        .add_ic_slot(IcKind::CallMethod, c, args.len() as u8)?;
                    self.cur().emit_ic_abc(Opcode::CallMethod, dst, base, ic);
                    self.cur().free_to(saved);
                    return Ok(());
                }
            }
        }

        if has_spread {
            let base = self.cur().alloc_regs(2)?; // base = callee, base+1 = receiver
            let args_arr = self.cur().alloc_reg()?;
            self.compile_callee_and_receiver(callee, base, base + 1, optional, dst)?;
            self.compile_spread_args_array(args, args_arr)?;
            self.cur()
                .emit(Instruction::abc(Opcode::CallSpread, dst, base, args_arr));
            self.cur().free_to(saved);
            return Ok(());
        }

        // Method call with computed key or optional `?.(...)`.
        if let ExprKind::Member { .. } = &callee.kind {
            let base = self.cur().alloc_regs((2 + args.len()) as u8)?;
            self.compile_callee_and_receiver(callee, base, base + 1, optional, dst)?;
            for (i, a) in args.iter().enumerate() {
                if let CallArg::Expr(e) = a {
                    self.compile_expr(e, base + 2 + i as u8)?;
                }
            }
            self.cur().emit(Instruction::abc(
                Opcode::CallWithThis,
                dst,
                base,
                args.len() as u8,
            ));
            self.cur().free_to(saved);
            return Ok(());
        }

        // Ordinary function call `f(a, b)` — check syntactic direct `eval(...)` call.
        let is_direct_eval = !optional && matches!(&callee.kind, ExprKind::Ident(n) if n == "eval");
        let base = self.cur().alloc_regs((1 + args.len()) as u8)?;
        self.compile_expr(callee, base)?;
        if optional {
            let j = self.cur().emit_jump(Opcode::JumpIfNullish, base);
            if let Some(exits) = self.cur().chain_exits.last_mut() {
                exits.push((j, dst));
            }
        }
        for (i, a) in args.iter().enumerate() {
            if let CallArg::Expr(e) = a {
                self.compile_expr(e, base + 1 + i as u8)?;
            }
        }
        if is_direct_eval {
            let mut forbidden_vars = Vec::new();
            for l in &self.cur().locals {
                if l.is_lex && !forbidden_vars.contains(&l.name) {
                    forbidden_vars.push(l.name.clone());
                }
            }
            if self.cur().in_param_init {
                let pnames = self.cur().param_names.clone();
                for p in pnames {
                    if !forbidden_vars.contains(&p) {
                        forbidden_vars.push(p);
                    }
                }
                if !self.cur().is_arrow && !forbidden_vars.iter().any(|n| n == "arguments") {
                    forbidden_vars.push("arguments".into());
                }
            }
            let in_function = self.frames.len() > 1;
            let meta = DirectEvalMeta {
                argc: args.len() as u8,
                is_strict: self.cur().is_strict,
                in_function,
                in_class_field_or_static: self.cur().in_class_field_or_static,
                in_class_body: self.cur().in_class_body,
                forbidden_vars,
            };
            let meta_c = self.cur().add_const(ConstantEntry::DirectEval(meta))?;
            if meta_c <= 255 {
                self.cur().emit(Instruction::abc(
                    Opcode::DirectEval,
                    dst,
                    base,
                    meta_c as u8,
                ));
                self.cur().free_to(saved);
                return Ok(());
            }
        }
        self.cur()
            .emit(Instruction::abc(Opcode::Call, dst, base, args.len() as u8));
        self.cur().free_to(saved);
        Ok(())
    }

    fn compile_callee_and_receiver(
        &mut self,
        callee: &Expr,
        callee_reg: u8,
        recv_reg: u8,
        optional_call: bool,
        chain_dst: u8,
    ) -> Result<(), CompileError> {
        if let ExprKind::Member {
            object,
            property,
            optional: mem_opt,
        } = &callee.kind
        {
            if matches!(object.kind, ExprKind::Super) {
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadThis, recv_reg, 0, 0));
                let saved = self.cur().next_reg;
                let key_reg = self.cur().alloc_reg()?;
                self.compile_member_prop_key(property, key_reg)?;
                self.cur().emit(Instruction::abc(
                    Opcode::GetSuperProp,
                    callee_reg,
                    key_reg,
                    0,
                ));
                self.cur().free_to(saved);
                return Ok(());
            }
            self.compile_expr(object, recv_reg)?;
            if *mem_opt {
                let j = self.cur().emit_jump(Opcode::JumpIfNullish, recv_reg);
                if let Some(exits) = self.cur().chain_exits.last_mut() {
                    exits.push((j, chain_dst));
                }
            }
            self.emit_member_load(callee_reg, recv_reg, property)?;
        } else {
            self.cur()
                .emit(Instruction::abc(Opcode::LoadUndef, recv_reg, 0, 0));
            self.compile_expr(callee, callee_reg)?;
        }
        if optional_call {
            let j = self.cur().emit_jump(Opcode::JumpIfNullish, callee_reg);
            if let Some(exits) = self.cur().chain_exits.last_mut() {
                exits.push((j, chain_dst));
            }
        }
        Ok(())
    }

    fn compile_new(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
        dst: u8,
    ) -> Result<(), CompileError> {
        let saved = self.cur().next_reg;
        let has_spread = args.iter().any(|a| matches!(a, CallArg::Spread(_)));
        if has_spread {
            let callee_reg = self.cur().alloc_reg()?;
            let args_arr = self.cur().alloc_reg()?;
            self.compile_expr(callee, callee_reg)?;
            self.compile_spread_args_array(args, args_arr)?;
            self.cur().emit(Instruction::abc(
                Opcode::ConstructSpread,
                dst,
                callee_reg,
                args_arr,
            ));
        } else {
            let base = self.cur().alloc_regs((1 + args.len()) as u8)?;
            self.compile_expr(callee, base)?;
            for (i, a) in args.iter().enumerate() {
                if let CallArg::Expr(e) = a {
                    self.compile_expr(e, base + 1 + i as u8)?;
                }
            }
            self.cur().emit(Instruction::abc(
                Opcode::Construct,
                dst,
                base,
                args.len() as u8,
            ));
        }
        self.cur().free_to(saved);
        Ok(())
    }

    fn compile_spread_args_array(
        &mut self,
        args: &[CallArg],
        arr_reg: u8,
    ) -> Result<(), CompileError> {
        self.cur()
            .emit(Instruction::abc(Opcode::NewArray, arr_reg, 0, 0));
        let saved = self.cur().next_reg;
        let tmp = self.cur().alloc_reg()?;
        for a in args {
            match a {
                CallArg::Expr(e) => {
                    self.compile_expr(e, tmp)?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::ArrayPush, arr_reg, tmp, 0));
                }
                CallArg::Spread(e) => {
                    self.compile_expr(e, tmp)?;
                    self.cur().emit(Instruction::abc(
                        Opcode::ArrayAppendIterable,
                        arr_reg,
                        tmp,
                        0,
                    ));
                }
            }
        }
        self.cur().free_to(saved);
        Ok(())
    }

    fn compile_unary(&mut self, op: UnaryOp, arg: &Expr, dst: u8) -> Result<(), CompileError> {
        match op {
            UnaryOp::TypeOf => {
                if let ExprKind::Ident(ref name) = arg.kind {
                    let frame_idx = self.frames.len() - 1;
                    if let VarResolution::Global(g) = self.resolve_var(frame_idx, name)? {
                        let c = self.cur().add_string_const(&g)?;
                        let ic = self.cur().add_ic_slot(IcKind::TypeOfGlobal, c, 0)?;
                        self.cur().emit_abx_wide(Opcode::TypeOfGlobal, dst, ic);
                        return Ok(());
                    }
                }
                self.compile_expr(arg, dst)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::TypeOf, dst, dst, 0));
            }
            UnaryOp::Void => {
                self.compile_expr(arg, dst)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::LoadUndef, dst, 0, 0));
            }
            UnaryOp::Delete => {
                if let ExprKind::Member {
                    object, property, ..
                } = &arg.kind
                {
                    let saved = self.cur().next_reg;
                    let obj = self.cur().alloc_reg()?;
                    let key = self.cur().alloc_reg()?;
                    self.compile_expr(object, obj)?;
                    self.compile_member_prop_key(property, key)?;
                    let del_op = match property {
                        MemberProp::Computed(_) => Opcode::DeleteElem,
                        _ => Opcode::DeleteProp,
                    };
                    self.cur().emit(Instruction::abc(del_op, dst, obj, key));
                    self.cur().free_to(saved);
                } else {
                    self.compile_expr(arg, dst)?;
                    self.cur()
                        .emit(Instruction::abc(Opcode::LoadTrue, dst, 0, 0));
                }
            }
            UnaryOp::Plus => {
                self.compile_expr(arg, dst)?;
                self.cur().emit(Instruction::abc(Opcode::Pos, dst, dst, 0));
            }
            UnaryOp::Minus => {
                self.compile_expr(arg, dst)?;
                self.cur().emit(Instruction::abc(Opcode::Neg, dst, dst, 0));
            }
            UnaryOp::Not => {
                self.compile_expr(arg, dst)?;
                self.cur().emit(Instruction::abc(Opcode::Not, dst, dst, 0));
            }
            UnaryOp::BitNot => {
                self.compile_expr(arg, dst)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::BitNot, dst, dst, 0));
            }
        }
        Ok(())
    }

    fn compile_update(
        &mut self,
        op: UpdateOp,
        arg: &Expr,
        prefix: bool,
        dst: u8,
    ) -> Result<(), CompileError> {
        let inc_op = if op == UpdateOp::Inc {
            Opcode::Inc
        } else {
            Opcode::Dec
        };
        let saved = self.cur().next_reg;
        match &arg.kind {
            ExprKind::Ident(name) => {
                let tmp = self.cur().alloc_reg()?;
                self.emit_load_var(name, tmp)?;
                if !prefix {
                    self.cur()
                        .emit(Instruction::abc(Opcode::ToNumber, dst, tmp, 0));
                }
                self.cur().emit(Instruction::abc(inc_op, tmp, tmp, 0));
                self.emit_store_var(name, tmp)?;
                if prefix {
                    self.cur().emit(Instruction::abc(Opcode::Move, dst, tmp, 0));
                }
            }
            ExprKind::Member {
                object, property, ..
            } => {
                let obj = self.cur().alloc_reg()?;
                let val = self.cur().alloc_reg()?;
                self.compile_expr(object, obj)?;
                self.emit_member_load(val, obj, property)?;
                if !prefix {
                    self.cur()
                        .emit(Instruction::abc(Opcode::ToNumber, dst, val, 0));
                }
                self.cur().emit(Instruction::abc(inc_op, val, val, 0));
                self.emit_member_store(obj, property, val)?;
                if prefix {
                    self.cur().emit(Instruction::abc(Opcode::Move, dst, val, 0));
                }
            }
            ExprKind::Paren(inner) => return self.compile_update(op, inner, prefix, dst),
            _ => {
                return Err(CompileError::Limit(
                    "Invalid update expression target".into(),
                ));
            }
        }
        self.cur().free_to(saved);
        Ok(())
    }

    fn compile_assign(
        &mut self,
        op: AssignOp,
        left: &AssignTarget,
        right: &Expr,
        dst: u8,
    ) -> Result<(), CompileError> {
        if op == AssignOp::Assign {
            let inferred = match left {
                AssignTarget::Ident { name, .. } => Some(name.as_str()),
                _ => None,
            };
            self.compile_expr_named(right, dst, inferred)?;
            self.compile_assign_target_store(left, dst)?;
            return Ok(());
        }

        // Logical short-circuit assignments (`&&=`, `||=`, `??=`).
        if matches!(
            op,
            AssignOp::AndAssign | AssignOp::OrAssign | AssignOp::NullishAssign
        ) {
            self.load_assign_target(left, dst)?;
            let skip_jump = match op {
                AssignOp::AndAssign => self.cur().emit_jump(Opcode::JumpIfFalse, dst),
                AssignOp::OrAssign => self.cur().emit_jump(Opcode::JumpIfTrue, dst),
                AssignOp::NullishAssign => {
                    let j_eval = self.cur().emit_jump(Opcode::JumpIfNullish, dst);
                    let j_done = self.cur().emit_jump(Opcode::Jump, 0);
                    let rhs_pc = self.cur().code.len();
                    self.cur().patch_jump(j_eval, rhs_pc)?;
                    self.compile_expr(right, dst)?;
                    self.compile_assign_target_store(left, dst)?;
                    let end_pc = self.cur().code.len();
                    self.cur().patch_jump(j_done, end_pc)?;
                    return Ok(());
                }
                _ => unreachable!(),
            };
            self.compile_expr(right, dst)?;
            self.compile_assign_target_store(left, dst)?;
            let end_pc = self.cur().code.len();
            self.cur().patch_jump(skip_jump, end_pc)?;
            return Ok(());
        }

        // Compound arithmetic/bitwise assignment (`+=`, `-=`, etc.).
        let bin_op = match op {
            AssignOp::AddAssign => Opcode::Add,
            AssignOp::SubAssign => Opcode::Sub,
            AssignOp::MulAssign => Opcode::Mul,
            AssignOp::DivAssign => Opcode::Div,
            AssignOp::RemAssign => Opcode::Rem,
            AssignOp::ExpAssign => Opcode::Exp,
            AssignOp::ShlAssign => Opcode::Shl,
            AssignOp::ShrAssign => Opcode::Shr,
            AssignOp::UShrAssign => Opcode::UShr,
            AssignOp::BitAndAssign => Opcode::BitAnd,
            AssignOp::BitOrAssign => Opcode::BitOr,
            AssignOp::BitXorAssign => Opcode::BitXor,
            _ => unreachable!(),
        };
        let saved = self.cur().next_reg;
        let lhs = self.cur().alloc_reg()?;
        let rhs = self.cur().alloc_reg()?;
        match left {
            AssignTarget::Ident { name, .. } => {
                self.emit_load_var(name, lhs)?;
                self.compile_expr(right, rhs)?;
                self.cur().emit(Instruction::abc(bin_op, dst, lhs, rhs));
                self.emit_store_var(name, dst)?;
            }
            AssignTarget::Member {
                object, property, ..
            } => {
                let obj = self.cur().alloc_reg()?;
                self.compile_expr(object, obj)?;
                self.emit_member_load(lhs, obj, property)?;
                self.compile_expr(right, rhs)?;
                self.cur().emit(Instruction::abc(bin_op, dst, lhs, rhs));
                self.emit_member_store(obj, property, dst)?;
            }
            AssignTarget::Pattern(_) => {
                return Err(CompileError::Limit(
                    "Compound assignment cannot target a destructuring pattern".into(),
                ));
            }
        }
        self.cur().free_to(saved);
        Ok(())
    }

    fn load_assign_target(&mut self, target: &AssignTarget, dst: u8) -> Result<(), CompileError> {
        match target {
            AssignTarget::Ident { name, .. } => self.emit_load_var(name, dst),
            AssignTarget::Member {
                object, property, ..
            } => {
                let saved = self.cur().next_reg;
                let obj = self.cur().alloc_reg()?;
                self.compile_expr(object, obj)?;
                self.emit_member_load(dst, obj, property)?;
                self.cur().free_to(saved);
                Ok(())
            }
            AssignTarget::Pattern(_) => Err(CompileError::Limit(
                "Cannot load value from destructuring pattern".into(),
            )),
        }
    }

    fn emit_member_load(
        &mut self,
        dst: u8,
        obj_reg: u8,
        prop: &MemberProp,
    ) -> Result<(), CompileError> {
        match prop {
            MemberProp::Ident(name) => {
                let c = self.cur().add_string_const(name)?;
                let ic = self.cur().add_ic_slot(IcKind::GetProp, c, 0)?;
                self.cur().emit_ic_abc(Opcode::GetProp, dst, obj_reg, ic);
            }
            MemberProp::Private(name) => {
                let c = self.cur().add_string_const(name)?;
                let ic = self.cur().add_ic_slot(IcKind::GetProp, c, 0)?;
                self.cur().emit_ic_abc(Opcode::GetPrivate, dst, obj_reg, ic);
            }
            MemberProp::Computed(expr) => {
                let saved = self.cur().next_reg;
                let key = self.cur().alloc_reg()?;
                self.compile_expr(expr, key)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::GetElem, dst, obj_reg, key));
                self.cur().free_to(saved);
            }
        }
        Ok(())
    }

    fn emit_member_store(
        &mut self,
        obj_reg: u8,
        prop: &MemberProp,
        val_reg: u8,
    ) -> Result<(), CompileError> {
        match prop {
            MemberProp::Ident(name) => {
                let c = self.cur().add_string_const(name)?;
                let ic = self.cur().add_ic_slot(IcKind::SetProp, c, 0)?;
                self.cur()
                    .emit_ic_abc(Opcode::SetProp, obj_reg, val_reg, ic);
            }
            MemberProp::Private(name) => {
                let c = self.cur().add_string_const(name)?;
                let ic = self.cur().add_ic_slot(IcKind::SetProp, c, 0)?;
                self.cur()
                    .emit_ic_abc(Opcode::SetPrivate, obj_reg, val_reg, ic);
            }
            MemberProp::Computed(expr) => {
                let saved = self.cur().next_reg;
                let key = self.cur().alloc_reg()?;
                self.compile_expr(expr, key)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::SetElem, obj_reg, key, val_reg));
                self.cur().free_to(saved);
            }
        }
        Ok(())
    }

    fn emit_prop_define(
        &mut self,
        obj_reg: u8,
        key: &PropKey,
        val_reg: u8,
    ) -> Result<(), CompileError> {
        match key {
            PropKey::Ident(name) | PropKey::String(name) => {
                let c = self.cur().add_string_const(name)?;
                let ic = self.cur().add_ic_slot(IcKind::DefineProp, c, 0)?;
                self.cur()
                    .emit_ic_abc(Opcode::DefineProp, obj_reg, val_reg, ic);
            }
            PropKey::Private(name) => {
                let c = self.cur().add_string_const(name)?;
                let ic = self.cur().add_ic_slot(IcKind::SetProp, c, 0)?;
                self.cur()
                    .emit_ic_abc(Opcode::DefinePrivate, obj_reg, val_reg, ic);
            }
            PropKey::Number(n) => {
                let c = self.cur().add_string_const(&n.to_string())?;
                let ic = self.cur().add_ic_slot(IcKind::DefineProp, c, 0)?;
                self.cur()
                    .emit_ic_abc(Opcode::DefineProp, obj_reg, val_reg, ic);
            }
            PropKey::Computed(expr) => {
                let saved = self.cur().next_reg;
                let key_reg = self.cur().alloc_reg()?;
                self.compile_expr(expr, key_reg)?;
                self.cur()
                    .emit(Instruction::abc(Opcode::SetElem, obj_reg, key_reg, val_reg));
                self.cur().free_to(saved);
            }
        }
        Ok(())
    }

    fn compile_prop_key_into_reg(&mut self, key: &PropKey, dst: u8) -> Result<(), CompileError> {
        match key {
            PropKey::Ident(s) | PropKey::String(s) | PropKey::Private(s) => {
                let c = self.cur().add_string_const(s)?;
                self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
            }
            PropKey::Number(n) => self.emit_number(*n, dst)?,
            PropKey::Computed(e) => self.compile_expr(e, dst)?,
        }
        Ok(())
    }

    fn compile_member_prop_key(&mut self, prop: &MemberProp, dst: u8) -> Result<(), CompileError> {
        match prop {
            MemberProp::Ident(s) | MemberProp::Private(s) => {
                let c = self.cur().add_string_const(s)?;
                self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
            }
            MemberProp::Computed(e) => self.compile_expr(e, dst)?,
        }
        Ok(())
    }

    fn emit_number(&mut self, n: f64, dst: u8) -> Result<(), CompileError> {
        // Fit small exact integers (excluding -0.0!) into compact `LoadInt` immediate.
        if n.is_finite() && n.fract() == 0.0 && !(n == 0.0 && n.is_sign_negative()) {
            let as_i64 = n as i64;
            if let Ok(imm) = i16::try_from(as_i64) {
                self.cur()
                    .emit(Instruction::asbx(Opcode::LoadInt, dst, imm));
                return Ok(());
            }
            if let Ok(i32_val) = i32::try_from(as_i64) {
                let c = self.cur().add_const(ConstantEntry::Int32(i32_val))?;
                self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
                return Ok(());
            }
        }
        let c = self.cur().add_const(ConstantEntry::Number(n))?;
        self.cur().emit_abx_wide(Opcode::LoadConst, dst, c);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForEachKind {
    In,
    Of,
    AsyncOf,
}

enum StaticInitItem<'a> {
    Field(&'a PropKey, Option<&'a Expr>),
    Block(&'a [Statement]),
}

fn collect_hoisted_vars(stmts: &[Statement], out: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            Statement::VarDecl {
                kind: VarKind::Var,
                decls,
                ..
            } => {
                for d in decls {
                    let mut names = Vec::new();
                    d.id.collect_bound_names(&mut names);
                    for (n, _) in names {
                        if !out.contains(&n) {
                            out.push(n);
                        }
                    }
                }
            }
            Statement::Block { body, .. } => collect_hoisted_vars(body, out),
            Statement::If {
                consequent,
                alternate,
                ..
            } => {
                collect_hoisted_vars(std::slice::from_ref(consequent.as_ref()), out);
                if let Some(alt) = alternate {
                    collect_hoisted_vars(std::slice::from_ref(alt.as_ref()), out);
                }
            }
            Statement::While { body, .. }
            | Statement::DoWhile { body, .. }
            | Statement::With { body, .. }
            | Statement::Labeled { body, .. } => {
                collect_hoisted_vars(std::slice::from_ref(body.as_ref()), out);
            }
            Statement::For { init, body, .. } => {
                if let Some(ForInit::VarDecl {
                    kind: VarKind::Var,
                    decls,
                    ..
                }) = init
                {
                    for d in decls {
                        let mut names = Vec::new();
                        d.id.collect_bound_names(&mut names);
                        for (n, _) in names {
                            if !out.contains(&n) {
                                out.push(n);
                            }
                        }
                    }
                }
                collect_hoisted_vars(std::slice::from_ref(body.as_ref()), out);
            }
            Statement::ForIn { left, body, .. } | Statement::ForOf { left, body, .. } => {
                if let ForEachLeft::VarDecl {
                    kind: VarKind::Var,
                    id,
                    ..
                } = left
                {
                    let mut names = Vec::new();
                    id.collect_bound_names(&mut names);
                    for (n, _) in names {
                        if !out.contains(&n) {
                            out.push(n);
                        }
                    }
                }
                collect_hoisted_vars(std::slice::from_ref(body.as_ref()), out);
            }
            Statement::Switch { cases, .. } => {
                for c in cases {
                    collect_hoisted_vars(&c.consequent, out);
                }
            }
            Statement::Try {
                block,
                handler,
                finalizer,
                ..
            } => {
                collect_hoisted_vars(block, out);
                if let Some(h) = handler {
                    collect_hoisted_vars(&h.body, out);
                }
                if let Some(f) = finalizer {
                    collect_hoisted_vars(f, out);
                }
            }
            Statement::ExportDecl(ExportDecl {
                kind: ExportKind::Declaration(inner),
                ..
            }) => collect_hoisted_vars(std::slice::from_ref(inner.as_ref()), out),
            _ => {}
        }
    }
}

/// Returns `true` if `func` (or any nested arrow function / direct `eval`)
/// references the identifier `arguments` within this function's own scope.
fn function_uses_arguments(func: &Function) -> bool {
    func.params.iter().any(pattern_uses_arguments)
        || func
            .rest_param
            .as_deref()
            .is_some_and(pattern_uses_arguments)
        || stmts_use_arguments(&func.body)
}

fn pattern_uses_arguments(pat: &Pattern) -> bool {
    match pat {
        Pattern::Ident { .. } => false,
        Pattern::Array { elements, rest, .. } => {
            elements.iter().flatten().any(pattern_uses_arguments)
                || rest.as_deref().is_some_and(pattern_uses_arguments)
        }
        Pattern::Object { props, rest, .. } => {
            props
                .iter()
                .any(|p| prop_key_uses_arguments(&p.key) || pattern_uses_arguments(&p.value))
                || rest.as_deref().is_some_and(pattern_uses_arguments)
        }
        Pattern::Assign { left, right, .. } => {
            pattern_uses_arguments(left) || expr_uses_arguments(right)
        }
    }
}

fn prop_key_uses_arguments(key: &PropKey) -> bool {
    matches!(key, PropKey::Computed(e) if expr_uses_arguments(e))
}

fn target_uses_arguments(target: &AssignTarget) -> bool {
    match target {
        AssignTarget::Ident { name, .. } => name == "arguments",
        AssignTarget::Member {
            object, property, ..
        } => {
            expr_uses_arguments(object)
                || matches!(property, MemberProp::Computed(e) if expr_uses_arguments(e))
        }
        AssignTarget::Pattern(p) => pattern_uses_arguments(p),
    }
}

fn stmts_use_arguments(stmts: &[Statement]) -> bool {
    stmts.iter().any(stmt_uses_arguments)
}

fn stmt_uses_arguments(stmt: &Statement) -> bool {
    match stmt {
        Statement::Block { body, .. } => stmts_use_arguments(body),
        Statement::VarDecl { decls, .. } => decls.iter().any(|d| {
            pattern_uses_arguments(&d.id) || d.init.as_ref().is_some_and(expr_uses_arguments)
        }),
        Statement::Expr { expr, .. } => expr_uses_arguments(expr),
        Statement::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            expr_uses_arguments(test)
                || stmt_uses_arguments(consequent)
                || alternate.as_deref().is_some_and(stmt_uses_arguments)
        }
        Statement::While { test, body, .. } | Statement::DoWhile { body, test, .. } => {
            expr_uses_arguments(test) || stmt_uses_arguments(body)
        }
        Statement::For {
            init,
            test,
            update,
            body,
            ..
        } => {
            init.as_ref().is_some_and(|i| match i {
                ForInit::VarDecl { decls, .. } => decls.iter().any(|d| {
                    pattern_uses_arguments(&d.id)
                        || d.init.as_ref().is_some_and(expr_uses_arguments)
                }),
                ForInit::Expr(e) => expr_uses_arguments(e),
            }) || test.as_ref().is_some_and(expr_uses_arguments)
                || update.as_ref().is_some_and(expr_uses_arguments)
                || stmt_uses_arguments(body)
        }
        Statement::ForIn {
            left, right, body, ..
        }
        | Statement::ForOf {
            left, right, body, ..
        } => {
            (match left {
                ForEachLeft::VarDecl { id, .. } => pattern_uses_arguments(id),
                ForEachLeft::Target(t) => target_uses_arguments(t),
            }) || expr_uses_arguments(right)
                || stmt_uses_arguments(body)
        }
        Statement::Switch {
            discriminant,
            cases,
            ..
        } => {
            expr_uses_arguments(discriminant)
                || cases.iter().any(|c| {
                    c.test.as_ref().is_some_and(expr_uses_arguments)
                        || stmts_use_arguments(&c.consequent)
                })
        }
        Statement::Return { arg, .. } => arg.as_ref().is_some_and(expr_uses_arguments),
        Statement::Throw { arg, .. } => expr_uses_arguments(arg),
        Statement::Try {
            block,
            handler,
            finalizer,
            ..
        } => {
            stmts_use_arguments(block)
                || handler.as_ref().is_some_and(|h| {
                    h.param.as_ref().is_some_and(pattern_uses_arguments)
                        || stmts_use_arguments(&h.body)
                })
                || finalizer.as_deref().is_some_and(stmts_use_arguments)
        }
        Statement::With { object, body, .. } => {
            expr_uses_arguments(object) || stmt_uses_arguments(body)
        }
        Statement::Labeled { body, .. } => stmt_uses_arguments(body),
        Statement::ClassDecl(cls) => class_uses_arguments(cls),
        // Non-arrow nested functions have their own `arguments` binding.
        Statement::FunctionDecl(_)
        | Statement::Empty { .. }
        | Statement::Break { .. }
        | Statement::Continue { .. }
        | Statement::Debugger { .. }
        | Statement::ImportDecl(_)
        | Statement::ExportDecl(_) => false,
    }
}

fn class_uses_arguments(cls: &Class) -> bool {
    cls.super_class.as_deref().is_some_and(expr_uses_arguments)
        || cls.elements.iter().any(|el| match el {
            ClassElement::Method { key, .. } => prop_key_uses_arguments(key),
            ClassElement::Field { key, value, .. } => {
                prop_key_uses_arguments(key) || value.as_ref().is_some_and(expr_uses_arguments)
            }
            ClassElement::StaticBlock { .. } => false,
        })
}

fn expr_uses_arguments(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Ident(n) => n == "arguments" || n == "eval",
        ExprKind::Array(elems) => elems.iter().flatten().any(|e| match e {
            ArrayElement::Expr(x) | ArrayElement::Spread(x) => expr_uses_arguments(x),
        }),
        ExprKind::Object(props) => props.iter().any(|p| match p {
            ObjectProp::KeyValue { key, value, .. } => {
                prop_key_uses_arguments(key) || expr_uses_arguments(value)
            }
            ObjectProp::Method { key, .. } => prop_key_uses_arguments(key),
            ObjectProp::Spread { arg, .. } => expr_uses_arguments(arg),
        }),
        ExprKind::Template { exprs, .. } => exprs.iter().any(expr_uses_arguments),
        ExprKind::TaggedTemplate { tag, exprs, .. } => {
            expr_uses_arguments(tag) || exprs.iter().any(expr_uses_arguments)
        }
        // Arrow functions inherit the enclosing function's `arguments`!
        ExprKind::Arrow {
            params,
            rest_param,
            body,
            ..
        } => {
            params.iter().any(pattern_uses_arguments)
                || rest_param.as_deref().is_some_and(pattern_uses_arguments)
                || match body {
                    ArrowBody::Block { body, .. } => stmts_use_arguments(body),
                    ArrowBody::Expr(e) => expr_uses_arguments(e),
                }
        }
        ExprKind::Class(cls) => class_uses_arguments(cls),
        ExprKind::Member {
            object, property, ..
        } => {
            expr_uses_arguments(object)
                || matches!(property, MemberProp::Computed(e) if expr_uses_arguments(e))
        }
        ExprKind::Call { callee, args, .. } | ExprKind::New { callee, args } => {
            expr_uses_arguments(callee)
                || args.iter().any(|a| match a {
                    CallArg::Expr(x) | CallArg::Spread(x) => expr_uses_arguments(x),
                })
        }
        ExprKind::ImportCall { source } => expr_uses_arguments(source),
        ExprKind::Unary { arg, .. }
        | ExprKind::Update { arg, .. }
        | ExprKind::Await(arg)
        | ExprKind::Chain(arg)
        | ExprKind::Paren(arg) => expr_uses_arguments(arg),
        ExprKind::Binary { left, right, .. } | ExprKind::Logical { left, right, .. } => {
            expr_uses_arguments(left) || expr_uses_arguments(right)
        }
        ExprKind::Conditional {
            test,
            consequent,
            alternate,
        } => {
            expr_uses_arguments(test)
                || expr_uses_arguments(consequent)
                || expr_uses_arguments(alternate)
        }
        ExprKind::Assign { left, right, .. } => {
            target_uses_arguments(left) || expr_uses_arguments(right)
        }
        ExprKind::Sequence(list) => list.iter().any(expr_uses_arguments),
        ExprKind::Yield { arg, .. } => arg.as_deref().is_some_and(expr_uses_arguments),
        ExprKind::Function(_)
        | ExprKind::Null
        | ExprKind::Bool(_)
        | ExprKind::Number(_)
        | ExprKind::BigInt(_)
        | ExprKind::String(_)
        | ExprKind::RegExp { .. }
        | ExprKind::PrivateIdent(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::NewTarget
        | ExprKind::ImportMeta => false,
    }
}

fn unwrap_labeled_func_decl(mut stmt: &Statement) -> Option<&Function> {
    while let Statement::Labeled { body, .. } = stmt {
        stmt = body.as_ref();
    }
    match stmt {
        Statement::FunctionDecl(f) => Some(f),
        _ => None,
    }
}

fn collect_direct_lex_names(stmts: &[Statement], out: &mut HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Statement::VarDecl {
                kind: VarKind::Let | VarKind::Const,
                decls,
                ..
            } => {
                for d in decls {
                    let mut names = Vec::new();
                    d.id.collect_bound_names(&mut names);
                    for (n, _) in names {
                        out.insert(n);
                    }
                }
            }
            Statement::ClassDecl(cls) => {
                if let Some(ref n) = cls.name {
                    out.insert(n.clone());
                }
            }
            Statement::FunctionDecl(f) if f.is_async || f.is_generator => {
                if let Some(ref n) = f.name {
                    out.insert(n.clone());
                }
            }
            _ => {}
        }
    }
}

/// Annex B §B.3.3: collects eligible sloppy-mode block-level plain `FunctionDeclaration` names
/// not shadowed by any enclosing lexical declaration.
fn collect_annex_b_block_funcs(
    stmts: &[Statement],
    blocked: &HashSet<String>,
    in_block: bool,
    out: &mut Vec<String>,
) {
    let mut local_blocked = blocked.clone();
    if in_block {
        collect_direct_lex_names(stmts, &mut local_blocked);
    }
    for stmt in stmts {
        match stmt {
            Statement::FunctionDecl(f) => {
                if in_block && !f.is_async && !f.is_generator {
                    if let Some(ref name) = f.name {
                        if !local_blocked.contains(name) && !out.contains(name) {
                            out.push(name.clone());
                        }
                    }
                }
            }
            Statement::Block { body, .. } => {
                collect_annex_b_block_funcs(body, &local_blocked, true, out);
            }
            Statement::If {
                consequent,
                alternate,
                ..
            } => {
                collect_annex_b_block_funcs(
                    std::slice::from_ref(consequent.as_ref()),
                    &local_blocked,
                    true,
                    out,
                );
                if let Some(alt) = alternate {
                    collect_annex_b_block_funcs(
                        std::slice::from_ref(alt.as_ref()),
                        &local_blocked,
                        true,
                        out,
                    );
                }
            }
            Statement::Switch { cases, .. } => {
                let mut sw_blocked = local_blocked.clone();
                for c in cases {
                    collect_direct_lex_names(&c.consequent, &mut sw_blocked);
                }
                for c in cases {
                    collect_annex_b_block_funcs(&c.consequent, &sw_blocked, true, out);
                }
            }
            Statement::Try {
                block,
                handler,
                finalizer,
                ..
            } => {
                collect_annex_b_block_funcs(block, &local_blocked, true, out);
                if let Some(h) = handler {
                    let mut catch_blocked = local_blocked.clone();
                    if let Some(ref p) = h.param {
                        if !matches!(p, Pattern::Ident { .. }) {
                            let mut names = Vec::new();
                            p.collect_bound_names(&mut names);
                            for (n, _) in names {
                                catch_blocked.insert(n);
                            }
                        }
                    }
                    collect_annex_b_block_funcs(&h.body, &catch_blocked, true, out);
                }
                if let Some(fin) = finalizer {
                    collect_annex_b_block_funcs(fin, &local_blocked, true, out);
                }
            }
            Statement::While { body, .. }
            | Statement::DoWhile { body, .. }
            | Statement::With { body, .. }
            | Statement::Labeled { body, .. } => {
                collect_annex_b_block_funcs(
                    std::slice::from_ref(body.as_ref()),
                    &local_blocked,
                    in_block,
                    out,
                );
            }
            Statement::For { init, body, .. } => {
                let mut for_blocked = local_blocked.clone();
                if let Some(ForInit::VarDecl {
                    kind: VarKind::Let | VarKind::Const,
                    decls,
                    ..
                }) = init
                {
                    for d in decls {
                        let mut names = Vec::new();
                        d.id.collect_bound_names(&mut names);
                        for (n, _) in names {
                            for_blocked.insert(n);
                        }
                    }
                }
                collect_annex_b_block_funcs(
                    std::slice::from_ref(body.as_ref()),
                    &for_blocked,
                    true,
                    out,
                );
            }
            Statement::ForIn { left, body, .. } | Statement::ForOf { left, body, .. } => {
                let mut for_blocked = local_blocked.clone();
                if let ForEachLeft::VarDecl {
                    kind: VarKind::Let | VarKind::Const,
                    id,
                    ..
                } = left
                {
                    let mut names = Vec::new();
                    id.collect_bound_names(&mut names);
                    for (n, _) in names {
                        for_blocked.insert(n);
                    }
                }
                collect_annex_b_block_funcs(
                    std::slice::from_ref(body.as_ref()),
                    &for_blocked,
                    true,
                    out,
                );
            }
            _ => {}
        }
    }
}
