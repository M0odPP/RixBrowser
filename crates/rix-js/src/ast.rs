//! ECMAScript 2024 Abstract Syntax Tree (AST) and syntax error types.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Byte range in the original UTF-8 source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[inline]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    #[inline]
    pub const fn merge(self, other: Self) -> Self {
        Self {
            start: if self.start < other.start {
                self.start
            } else {
                other.start
            },
            end: if self.end > other.end {
                self.end
            } else {
                other.end
            },
        }
    }
}

/// Parsing goal (`Script` vs ES `Module`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceType {
    Script,
    Module,
}

/// Syntax or early-error failure emitted by the lexer or parser.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("SyntaxError at byte {span_start}: {message}")]
pub struct SyntaxError {
    pub message: String,
    pub span_start: u32,
    pub span_end: u32,
}

impl SyntaxError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span_start: span.start,
            span_end: span.end,
        }
    }
}

/// Top-level parsed ECMAScript program.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub source_type: SourceType,
    pub strict: bool,
    pub body: Vec<Statement>,
    pub span: Span,
}

/// Variable binding kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarDeclarator {
    pub id: Pattern,
    pub init: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ForInit {
    VarDecl {
        kind: VarKind,
        decls: Vec<VarDeclarator>,
        span: Span,
    },
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ForEachLeft {
    VarDecl {
        kind: VarKind,
        id: Pattern,
        span: Span,
    },
    Target(AssignTarget),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    /// `None` denotes `default:`.
    pub test: Option<Expr>,
    pub consequent: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    pub param: Option<Pattern>,
    pub body: Vec<Statement>,
    pub span: Span,
}

/// Statement or module item.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Block {
        body: Vec<Statement>,
        span: Span,
    },
    VarDecl {
        kind: VarKind,
        decls: Vec<VarDeclarator>,
        span: Span,
    },
    Empty {
        span: Span,
    },
    Expr {
        expr: Expr,
        span: Span,
    },
    If {
        test: Expr,
        consequent: Box<Statement>,
        alternate: Option<Box<Statement>>,
        span: Span,
    },
    While {
        test: Expr,
        body: Box<Statement>,
        span: Span,
    },
    DoWhile {
        body: Box<Statement>,
        test: Expr,
        span: Span,
    },
    For {
        init: Option<ForInit>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Box<Statement>,
        span: Span,
    },
    ForIn {
        left: ForEachLeft,
        right: Expr,
        body: Box<Statement>,
        span: Span,
    },
    ForOf {
        is_await: bool,
        left: ForEachLeft,
        right: Expr,
        body: Box<Statement>,
        span: Span,
    },
    Switch {
        discriminant: Expr,
        cases: Vec<SwitchCase>,
        span: Span,
    },
    Break {
        label: Option<String>,
        span: Span,
    },
    Continue {
        label: Option<String>,
        span: Span,
    },
    Return {
        arg: Option<Expr>,
        span: Span,
    },
    Throw {
        arg: Expr,
        span: Span,
    },
    Try {
        block: Vec<Statement>,
        handler: Option<CatchClause>,
        finalizer: Option<Vec<Statement>>,
        span: Span,
    },
    With {
        object: Expr,
        body: Box<Statement>,
        span: Span,
    },
    Labeled {
        label: String,
        body: Box<Statement>,
        span: Span,
    },
    Debugger {
        span: Span,
    },
    FunctionDecl(Function),
    ClassDecl(Class),
    ImportDecl(ImportDecl),
    ExportDecl(ExportDecl),
}

impl Statement {
    pub fn span(&self) -> Span {
        match self {
            Statement::Block { span, .. }
            | Statement::VarDecl { span, .. }
            | Statement::Empty { span }
            | Statement::Expr { span, .. }
            | Statement::If { span, .. }
            | Statement::While { span, .. }
            | Statement::DoWhile { span, .. }
            | Statement::For { span, .. }
            | Statement::ForIn { span, .. }
            | Statement::ForOf { span, .. }
            | Statement::Switch { span, .. }
            | Statement::Break { span, .. }
            | Statement::Continue { span, .. }
            | Statement::Return { span, .. }
            | Statement::Throw { span, .. }
            | Statement::Try { span, .. }
            | Statement::With { span, .. }
            | Statement::Labeled { span, .. }
            | Statement::Debugger { span } => *span,
            Statement::FunctionDecl(f) => f.span,
            Statement::ClassDecl(c) => c.span,
            Statement::ImportDecl(i) => i.span,
            Statement::ExportDecl(e) => e.span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: Option<String>,
    pub params: Vec<Pattern>,
    pub rest_param: Option<Box<Pattern>>,
    pub body: Vec<Statement>,
    pub is_async: bool,
    pub is_generator: bool,
    pub strict: bool,
    pub has_simple_params: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrowBody {
    Block { body: Vec<Statement>, strict: bool },
    Expr(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub name: Option<String>,
    pub super_class: Option<Box<Expr>>,
    pub elements: Vec<ClassElement>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Constructor,
    Method,
    Get,
    Set,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropKey {
    Ident(String),
    Private(String),
    String(String),
    Number(f64),
    Computed(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClassElement {
    Method {
        key: PropKey,
        kind: MethodKind,
        is_static: bool,
        function: Function,
        span: Span,
    },
    Field {
        key: PropKey,
        is_static: bool,
        value: Option<Expr>,
        span: Span,
    },
    StaticBlock {
        body: Vec<Statement>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportDecl {
    pub specifiers: Vec<ImportSpecifier>,
    pub source: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportSpecifier {
    Default(String),
    Namespace(String),
    Named { imported: String, local: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportDecl {
    pub kind: ExportKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExportKind {
    Named {
        specifiers: Vec<ExportSpecifier>,
        source: Option<String>,
    },
    Declaration(Box<Statement>),
    DefaultExpr(Box<Expr>),
    DefaultFunction(Function),
    DefaultClass(Class),
    All {
        exported: Option<String>,
        source: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportSpecifier {
    pub local: String,
    pub exported: String,
}

/// Destructuring and parameter binding patterns.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Ident {
        name: String,
        span: Span,
    },
    Array {
        elements: Vec<Option<Pattern>>,
        rest: Option<Box<Pattern>>,
        span: Span,
    },
    Object {
        props: Vec<ObjectPatProp>,
        rest: Option<Box<Pattern>>,
        span: Span,
    },
    Assign {
        left: Box<Pattern>,
        right: Box<Expr>,
        span: Span,
    },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Ident { span, .. }
            | Pattern::Array { span, .. }
            | Pattern::Object { span, .. }
            | Pattern::Assign { span, .. } => *span,
        }
    }

    /// Collects all bound identifier names inside this pattern.
    pub fn collect_bound_names(&self, out: &mut Vec<(String, Span)>) {
        match self {
            Pattern::Ident { name, span } => out.push((name.clone(), *span)),
            Pattern::Array { elements, rest, .. } => {
                for el in elements.iter().flatten() {
                    el.collect_bound_names(out);
                }
                if let Some(rest) = rest {
                    rest.collect_bound_names(out);
                }
            }
            Pattern::Object { props, rest, .. } => {
                for prop in props {
                    prop.value.collect_bound_names(out);
                }
                if let Some(rest) = rest {
                    rest.collect_bound_names(out);
                }
            }
            Pattern::Assign { left, .. } => left.collect_bound_names(out),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPatProp {
    pub key: PropKey,
    pub value: Pattern,
    pub shorthand: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AssignTarget {
    Ident {
        name: String,
        span: Span,
    },
    Member {
        object: Box<Expr>,
        property: MemberProp,
        span: Span,
    },
    Pattern(Pattern),
}

impl AssignTarget {
    pub fn span(&self) -> Span {
        match self {
            AssignTarget::Ident { span, .. } | AssignTarget::Member { span, .. } => *span,
            AssignTarget::Pattern(p) => p.span(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemberProp {
    Ident(String),
    Private(String),
    Computed(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrayElement {
    Expr(Expr),
    Spread(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallArg {
    Expr(Expr),
    Spread(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObjectProp {
    KeyValue {
        key: PropKey,
        value: Expr,
        shorthand: bool,
        span: Span,
    },
    Method {
        key: PropKey,
        kind: MethodKind,
        function: Function,
        span: Span,
    },
    Spread {
        arg: Expr,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TemplateElement {
    pub raw: String,
    pub cooked: Option<String>,
    pub tail: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
    BitNot,
    TypeOf,
    Void,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    Inc,
    Dec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Exp,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    Eq,
    NotEq,
    StrictEq,
    StrictNotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    In,
    InstanceOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
    Nullish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    ExpAssign,
    ShlAssign,
    ShrAssign,
    UShrAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    AndAssign,
    OrAssign,
    NullishAssign,
}

/// Expression node with its source span.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    #[inline]
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind, span }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Null,
    Bool(bool),
    Number(f64),
    BigInt(String),
    String(String),
    RegExp {
        pattern: String,
        flags: String,
    },
    Ident(String),
    PrivateIdent(String),
    This,
    Super,
    NewTarget,
    ImportMeta,
    Array(Vec<Option<ArrayElement>>),
    Object(Vec<ObjectProp>),
    Template {
        quasis: Vec<TemplateElement>,
        exprs: Vec<Expr>,
    },
    TaggedTemplate {
        tag: Box<Expr>,
        quasis: Vec<TemplateElement>,
        exprs: Vec<Expr>,
    },
    Function(Box<Function>),
    Arrow {
        params: Vec<Pattern>,
        rest_param: Option<Box<Pattern>>,
        body: ArrowBody,
        is_async: bool,
        has_simple_params: bool,
    },
    Class(Box<Class>),
    Member {
        object: Box<Expr>,
        property: MemberProp,
        optional: bool,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<CallArg>,
        optional: bool,
    },
    New {
        callee: Box<Expr>,
        args: Vec<CallArg>,
    },
    ImportCall {
        source: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        arg: Box<Expr>,
    },
    Update {
        op: UpdateOp,
        arg: Box<Expr>,
        prefix: bool,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Logical {
        op: LogicalOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Conditional {
        test: Box<Expr>,
        consequent: Box<Expr>,
        alternate: Box<Expr>,
    },
    Assign {
        op: AssignOp,
        left: AssignTarget,
        right: Box<Expr>,
    },
    Sequence(Vec<Expr>),
    Await(Box<Expr>),
    Yield {
        arg: Option<Box<Expr>>,
        delegate: bool,
    },
    /// Groups an optional chaining expression (`a?.b.c()`) so short-circuit
    /// termination stops cleanly at the chain boundary.
    Chain(Box<Expr>),
    /// Parenthesized expression preserved during parsing so cover grammar
    /// (arrow params, destructuring, `??` with `&&`/`||`) can distinguish
    /// parenthesized expressions accurately.
    Paren(Box<Expr>),
}
