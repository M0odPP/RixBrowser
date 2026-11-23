//! Compact 32-bit fixed-width Register Bytecode IR for `rix-js`.
//!
//! Instruction bit layouts (little-endian `u32`):
//! - `ABC`:  `[op: 8 | a: 8 | b: 8 | c: 8]`
//! - `ABx`:  `[op: 8 | a: 8 | bx: 16]`
//! - `AsBx`: `[op: 8 | a: 8 | sbx: i16]` (relative PC offset from `pc + 1`)
//! - `Ax`:   `[op: 8 | ax: 24]` (extended operand for wide IC/constant indices)

use crate::ast::{MethodKind, SourceType};
use serde::{Deserialize, Serialize};

/// Monotonically bumped whenever instruction encoding or bytecode metadata changes.
pub const BYTECODE_VERSION: u32 = 1;

/// Magic header baked into serialized bytecode cache payloads.
pub const BYTECODE_CACHE_MAGIC: [u8; 4] = *b"RXBC";

/// Fixed-width 32-bit register bytecode instruction.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Instruction(pub u32);

impl Instruction {
    #[inline]
    pub const fn abc(op: Opcode, a: u8, b: u8, c: u8) -> Self {
        Self((op as u32) | ((a as u32) << 8) | ((b as u32) << 16) | ((c as u32) << 24))
    }

    #[inline]
    pub const fn abx(op: Opcode, a: u8, bx: u16) -> Self {
        Self((op as u32) | ((a as u32) << 8) | ((bx as u32) << 16))
    }

    #[inline]
    pub const fn asbx(op: Opcode, a: u8, sbx: i16) -> Self {
        Self((op as u32) | ((a as u32) << 8) | (((sbx as u16) as u32) << 16))
    }

    #[inline]
    pub const fn ax(op: Opcode, ax: u32) -> Self {
        Self((op as u32) | ((ax & 0x00FF_FFFF) << 8))
    }

    #[inline]
    pub const fn raw_opcode(self) -> u8 {
        (self.0 & 0xFF) as u8
    }

    #[inline]
    pub fn opcode(self) -> Option<Opcode> {
        Opcode::from_u8(self.raw_opcode())
    }

    #[inline]
    pub const fn a(self) -> u8 {
        ((self.0 >> 8) & 0xFF) as u8
    }

    #[inline]
    pub const fn b(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }

    #[inline]
    pub const fn c(self) -> u8 {
        ((self.0 >> 24) & 0xFF) as u8
    }

    #[inline]
    pub const fn bx(self) -> u16 {
        ((self.0 >> 16) & 0xFFFF) as u16
    }

    #[inline]
    pub const fn sbx(self) -> i16 {
        ((self.0 >> 16) & 0xFFFF) as u16 as i16
    }

    #[inline]
    pub const fn ax_val(self) -> u32 {
        self.0 >> 8
    }
}

/// Sentinel IC byte in 8-bit `ABC` property/call instructions indicating
/// the next instruction is `Opcode::ExtraArg(ic_idx)` for wide IC tables (`>= 255`).
pub const WIDE_IC_SENTINEL: u8 = 0xFF;

/// Register VM opcodes (`#[repr(u8)]`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Opcode {
    Nop = 0,
    ExtraArg = 1,

    // Register moves & literal loads
    Move = 2,
    LoadUndef = 3,
    LoadNull = 4,
    LoadTrue = 5,
    LoadFalse = 6,
    LoadInt = 7,
    LoadConst = 8,
    LoadThis = 9,
    LoadNewTarget = 10,
    LoadImportMeta = 11,
    LoadTdz = 12,
    CheckTdz = 13,

    // Globals, closures & upvalues
    GetGlobal = 14,
    SetGlobal = 15,
    DeclareGlobal = 16,
    TypeOfGlobal = 17,
    GetUpvalue = 18,
    SetUpvalue = 19,
    CloseUpvalues = 20,
    NewClosure = 21,

    // Objects, arrays, classes & properties (IC-indexed)
    NewObject = 22,
    NewArray = 23,
    ArrayPush = 24,
    ArrayPushHole = 25,
    ArrayAppendIterable = 26,
    ObjectCopyProps = 27,
    NewRegExp = 28,
    CreateClass = 29,
    GetProp = 30,
    SetProp = 31,
    DefineProp = 32,
    GetElem = 33,
    SetElem = 34,
    DeleteProp = 35,
    DeleteElem = 36,
    DefineGetter = 37,
    DefineSetter = 38,
    GetPrivate = 39,
    SetPrivate = 40,
    InPrivate = 41,
    GetSuperProp = 42,
    SetSuperProp = 43,

    // Calls & construction
    Call = 44,
    CallMethod = 45,
    CallWithThis = 46,
    CallSpread = 47,
    Construct = 48,
    ConstructSpread = 49,
    SuperConstruct = 50,

    // Arithmetic & bitwise binary ops
    Add = 51,
    Sub = 52,
    Mul = 53,
    Div = 54,
    Rem = 55,
    Exp = 56,
    BitAnd = 57,
    BitOr = 58,
    BitXor = 59,
    Shl = 60,
    Shr = 61,
    UShr = 62,

    // Comparisons & relational ops
    Eq = 63,
    NotEq = 64,
    StrictEq = 65,
    StrictNotEq = 66,
    Lt = 67,
    LtEq = 68,
    Gt = 69,
    GtEq = 70,
    In = 71,
    InstanceOf = 72,

    // Unary & conversion ops
    Neg = 73,
    Pos = 74,
    Not = 75,
    BitNot = 76,
    TypeOf = 77,
    ToNumber = 78,
    ToString = 79,
    Inc = 80,
    Dec = 81,

    // Control flow & OSR back-edge
    Jump = 82,
    JumpIfFalse = 83,
    JumpIfTrue = 84,
    JumpIfNullish = 85,
    JumpIfNotUndef = 86,
    LoopBack = 87,
    Return = 88,
    Throw = 89,

    // Iterators, async/await, generators & modules
    GetIterator = 90,
    GetAsyncIterator = 91,
    IteratorNext = 92,
    IteratorClose = 93,
    ForInKeys = 94,
    Await = 95,
    Yield = 96,
    YieldStar = 97,
    DynamicImport = 98,
    GetModuleImport = 99,
    SetModuleExport = 100,

    // Misc
    PushWith = 101,
    PopWith = 102,
    Debugger = 103,
    /// Collects remaining arguments `[first_idx..]` into a rest Array stored in `dst`.
    CollectRestArgs = 104,
    /// Materializes the ECMAScript `arguments` object for the current non-arrow call frame into `dst`.
    LoadArguments = 105,
    /// Invokes `next()` on an async iterator (or async-from-sync iterator) and stores the resulting Promise/result in `dst`.
    AsyncIteratorNext = 106,
    /// Unpacks an awaited `{ value, done }` iterator result object in `b` into `a` (value) and `c` (done boolean).
    UnpackIterResult = 107,
    /// Defines a new private field on object `a` with value `b` and IC slot `c` (`PrivateFieldAdd`).
    DefinePrivate = 108,
}

impl Opcode {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Nop),
            1 => Some(Self::ExtraArg),
            2 => Some(Self::Move),
            3 => Some(Self::LoadUndef),
            4 => Some(Self::LoadNull),
            5 => Some(Self::LoadTrue),
            6 => Some(Self::LoadFalse),
            7 => Some(Self::LoadInt),
            8 => Some(Self::LoadConst),
            9 => Some(Self::LoadThis),
            10 => Some(Self::LoadNewTarget),
            11 => Some(Self::LoadImportMeta),
            12 => Some(Self::LoadTdz),
            13 => Some(Self::CheckTdz),
            14 => Some(Self::GetGlobal),
            15 => Some(Self::SetGlobal),
            16 => Some(Self::DeclareGlobal),
            17 => Some(Self::TypeOfGlobal),
            18 => Some(Self::GetUpvalue),
            19 => Some(Self::SetUpvalue),
            20 => Some(Self::CloseUpvalues),
            21 => Some(Self::NewClosure),
            22 => Some(Self::NewObject),
            23 => Some(Self::NewArray),
            24 => Some(Self::ArrayPush),
            25 => Some(Self::ArrayPushHole),
            26 => Some(Self::ArrayAppendIterable),
            27 => Some(Self::ObjectCopyProps),
            28 => Some(Self::NewRegExp),
            29 => Some(Self::CreateClass),
            30 => Some(Self::GetProp),
            31 => Some(Self::SetProp),
            32 => Some(Self::DefineProp),
            33 => Some(Self::GetElem),
            34 => Some(Self::SetElem),
            35 => Some(Self::DeleteProp),
            36 => Some(Self::DeleteElem),
            37 => Some(Self::DefineGetter),
            38 => Some(Self::DefineSetter),
            39 => Some(Self::GetPrivate),
            40 => Some(Self::SetPrivate),
            41 => Some(Self::InPrivate),
            42 => Some(Self::GetSuperProp),
            43 => Some(Self::SetSuperProp),
            44 => Some(Self::Call),
            45 => Some(Self::CallMethod),
            46 => Some(Self::CallWithThis),
            47 => Some(Self::CallSpread),
            48 => Some(Self::Construct),
            49 => Some(Self::ConstructSpread),
            50 => Some(Self::SuperConstruct),
            51 => Some(Self::Add),
            52 => Some(Self::Sub),
            53 => Some(Self::Mul),
            54 => Some(Self::Div),
            55 => Some(Self::Rem),
            56 => Some(Self::Exp),
            57 => Some(Self::BitAnd),
            58 => Some(Self::BitOr),
            59 => Some(Self::BitXor),
            60 => Some(Self::Shl),
            61 => Some(Self::Shr),
            62 => Some(Self::UShr),
            63 => Some(Self::Eq),
            64 => Some(Self::NotEq),
            65 => Some(Self::StrictEq),
            66 => Some(Self::StrictNotEq),
            67 => Some(Self::Lt),
            68 => Some(Self::LtEq),
            69 => Some(Self::Gt),
            70 => Some(Self::GtEq),
            71 => Some(Self::In),
            72 => Some(Self::InstanceOf),
            73 => Some(Self::Neg),
            74 => Some(Self::Pos),
            75 => Some(Self::Not),
            76 => Some(Self::BitNot),
            77 => Some(Self::TypeOf),
            78 => Some(Self::ToNumber),
            79 => Some(Self::ToString),
            80 => Some(Self::Inc),
            81 => Some(Self::Dec),
            82 => Some(Self::Jump),
            83 => Some(Self::JumpIfFalse),
            84 => Some(Self::JumpIfTrue),
            85 => Some(Self::JumpIfNullish),
            86 => Some(Self::JumpIfNotUndef),
            87 => Some(Self::LoopBack),
            88 => Some(Self::Return),
            89 => Some(Self::Throw),
            90 => Some(Self::GetIterator),
            91 => Some(Self::GetAsyncIterator),
            92 => Some(Self::IteratorNext),
            93 => Some(Self::IteratorClose),
            94 => Some(Self::ForInKeys),
            95 => Some(Self::Await),
            96 => Some(Self::Yield),
            97 => Some(Self::YieldStar),
            98 => Some(Self::DynamicImport),
            99 => Some(Self::GetModuleImport),
            100 => Some(Self::SetModuleExport),
            101 => Some(Self::PushWith),
            102 => Some(Self::PopWith),
            103 => Some(Self::Debugger),
            104 => Some(Self::CollectRestArgs),
            105 => Some(Self::LoadArguments),
            106 => Some(Self::AsyncIteratorNext),
            107 => Some(Self::UnpackIterResult),
            108 => Some(Self::DefinePrivate),
            _ => None,
        }
    }
}

/// Inline cache site category recorded at compile time on every property/global/call op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IcKind {
    GetProp,
    SetProp,
    DefineProp,
    CallMethod,
    GetGlobal,
    SetGlobal,
    TypeOfGlobal,
}

/// Compile-time descriptor for an inline cache slot inside a [`FunctionBytecode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IcSlotDescriptor {
    pub kind: IcKind,
    /// Index into `FunctionBytecode::constants` holding the property/global name string.
    pub key_const: u32,
    /// Argument count for `CallMethod` sites (0 for property/global sites).
    pub argc: u8,
}

/// Exception handler range (`try` -> `catch` or `finally`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionHandler {
    pub try_start: u32,
    pub try_end: u32,
    pub handler_pc: u32,
    /// Register receiving the thrown exception value.
    pub catch_reg: u8,
    pub is_finally: bool,
}

/// Closure upvalue capture descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpvalueDescriptor {
    /// True if capturing a register directly in the enclosing function frame;
    /// false if forwarding an upvalue from the enclosing closure.
    pub is_local: bool,
    pub index: u8,
    pub is_const: bool,
}

/// Serializable method entry on a [`ClassBlueprint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassMethodBlueprint {
    pub name: String,
    pub is_private: bool,
    pub is_static: bool,
    pub kind: SerializableMethodKind,
    /// Index into `CompiledProgram::functions`.
    pub func_idx: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerializableMethodKind {
    Constructor,
    Method,
    Get,
    Set,
}

impl From<MethodKind> for SerializableMethodKind {
    fn from(k: MethodKind) -> Self {
        match k {
            MethodKind::Constructor => Self::Constructor,
            MethodKind::Method => Self::Method,
            MethodKind::Get => Self::Get,
            MethodKind::Set => Self::Set,
        }
    }
}

/// Serializable class definition metadata embedded in a constant pool entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassBlueprint {
    pub name: Option<String>,
    pub has_super: bool,
    pub constructor_idx: Option<u32>,
    pub methods: Vec<ClassMethodBlueprint>,
    pub instance_field_init_idx: Option<u32>,
    pub static_init_idx: Option<u32>,
}

/// Serializable constant pool entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConstantEntry {
    Number(f64),
    Int32(i32),
    String(String),
    BigInt(String),
    RegExp { pattern: String, flags: String },
    Function(u32),
    Class(ClassBlueprint),
}

/// Compiled register bytecode for a single JS function, script, or module body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionBytecode {
    pub name: Option<String>,
    pub param_count: u16,
    pub register_count: u16,
    pub is_strict: bool,
    pub is_async: bool,
    pub is_generator: bool,
    pub is_arrow: bool,
    pub code: Vec<Instruction>,
    pub constants: Vec<ConstantEntry>,
    pub ic_slots: Vec<IcSlotDescriptor>,
    pub upvalues: Vec<UpvalueDescriptor>,
    pub exception_handlers: Vec<ExceptionHandler>,
}

/// Static ES module import descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleImportEntry {
    pub specifier: String,
    pub imported_name: ModuleImportKind,
    pub local_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModuleImportKind {
    Default,
    Namespace,
    Named(String),
    SideEffectOnly,
}

/// Static ES module export descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleExportEntry {
    pub local_name: Option<String>,
    pub exported_name: String,
    pub reexport_from: Option<String>,
    pub is_star: bool,
}

/// Module linking metadata produced when compiling `SourceType::Module`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModuleBlueprint {
    pub imports: Vec<ModuleImportEntry>,
    pub exports: Vec<ModuleExportEntry>,
    pub has_top_level_await: bool,
}

/// Complete compiled program ready for Tier-1 VM execution or persistent caching.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledProgram {
    pub bytecode_version: u32,
    pub source_type: SourceType,
    pub entry_func: u32,
    pub functions: Vec<FunctionBytecode>,
    pub module: Option<ModuleBlueprint>,
}

impl CompiledProgram {
    #[inline]
    pub fn entry(&self) -> &FunctionBytecode {
        &self.functions[self.entry_func as usize]
    }
}
