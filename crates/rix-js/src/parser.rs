//! Spec-compliant ECMAScript 2024 Pratt & recursive-descent Parser.
//!
//! Performs full ES2024 grammar parsing (scripts, ES modules, classes,
//! destructuring, rest/spread, optional chaining, async/await, generators,
//! template literals, regex literals, ASI) along with strict-mode and
//! static early-error validation.

use crate::ast::*;
use crate::lexer::{is_strict_reserved_word, Keyword, Lexer, LexerCheckpoint, Token, TokenKind};
use std::collections::HashSet;

type FormalParams = (Vec<Pattern>, Option<Box<Pattern>>, bool);

#[derive(Debug, Clone)]
struct ParserCheckpoint {
    lexer: LexerCheckpoint,
    cur: Token,
    peek: Option<Token>,
}

#[derive(Debug, Clone)]
struct ScopeContext {
    strict: bool,
    in_function: bool,
    in_async: bool,
    in_generator: bool,
    in_loop: bool,
    in_switch: bool,
    in_class_body: bool,
    in_method: bool,
    in_derived_constructor: bool,
    allow_in: bool,
    labels: Vec<(String, bool)>,
    lex_scopes: Vec<HashSet<String>>,
    var_scopes: Vec<HashSet<String>>,
}

impl ScopeContext {
    fn new(source_type: SourceType) -> Self {
        Self {
            strict: source_type == SourceType::Module,
            in_function: false,
            in_async: source_type == SourceType::Module, // top-level await in ES modules
            in_generator: false,
            in_loop: false,
            in_switch: false,
            in_class_body: false,
            in_method: false,
            in_derived_constructor: false,
            allow_in: true,
            labels: Vec::new(),
            lex_scopes: vec![HashSet::new()],
            var_scopes: vec![HashSet::new()],
        }
    }
}

/// Parses an ECMAScript 2024 `Script` or `Module` into an AST [`Program`].
pub fn parse(source: &str, source_type: SourceType) -> Result<Program, SyntaxError> {
    let mut parser = Parser::new(source, source_type)?;
    parser.parse_program()
}

/// Parses `eval` source with caller strictness context.
pub fn parse_eval(
    source: &str,
    force_strict: bool,
    _in_class_field_or_static_block: bool,
    _allow_unbound_private: bool,
) -> Result<Program, SyntaxError> {
    let mut parser = Parser::new(source, SourceType::Script)?;
    if force_strict {
        parser.ctx.strict = true;
    }
    let mut prog = parser.parse_program()?;
    if force_strict {
        prog.strict = true;
    }
    Ok(prog)
}

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    cur: Token,
    peek: Option<Token>,
    source_type: SourceType,
    ctx: ScopeContext,
    recursion_depth: u32,
}

const MAX_PARSE_DEPTH: u32 = 192;

impl<'a> Parser<'a> {
    pub fn new(source: &'a str, source_type: SourceType) -> Result<Self, SyntaxError> {
        let mut lexer = Lexer::new_for_source(source, source_type == SourceType::Script);
        let cur = lexer.next_token()?;
        Ok(Self {
            lexer,
            cur,
            peek: None,
            source_type,
            ctx: ScopeContext::new(source_type),
            recursion_depth: 0,
        })
    }

    #[inline]
    fn with_depth<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, SyntaxError>,
    ) -> Result<T, SyntaxError> {
        if self.recursion_depth >= MAX_PARSE_DEPTH {
            return Err(SyntaxError {
                message: "Maximum parser recursion depth exceeded".into(),
                span_start: self.cur.span.start,
                span_end: self.cur.span.end,
            });
        }
        self.recursion_depth += 1;
        let res = f(self);
        self.recursion_depth -= 1;
        res
    }

    fn checkpoint(&self) -> ParserCheckpoint {
        ParserCheckpoint {
            lexer: self.lexer.checkpoint(),
            cur: self.cur.clone(),
            peek: self.peek.clone(),
        }
    }

    fn restore(&mut self, cp: ParserCheckpoint) {
        self.lexer.restore(cp.lexer);
        self.cur = cp.cur;
        self.peek = cp.peek;
    }

    fn advance(&mut self) -> Result<Token, SyntaxError> {
        let next = match self.peek.take() {
            Some(tok) => tok,
            None => self.lexer.next_token()?,
        };
        Ok(std::mem::replace(&mut self.cur, next))
    }

    fn peek_token(&mut self) -> Result<&Token, SyntaxError> {
        if self.peek.is_none() {
            self.peek = Some(self.lexer.next_token()?);
        }
        Ok(self.peek.as_ref().expect("peek populated"))
    }

    fn expect(&mut self, expected: TokenKind) -> Result<Token, SyntaxError> {
        if self.cur.kind == expected {
            self.advance()
        } else {
            Err(SyntaxError::new(
                format!("Expected {expected:?}, found {:?}", self.cur.kind),
                self.cur.span,
            ))
        }
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<Token, SyntaxError> {
        if self.cur.kind == TokenKind::Keyword(kw) {
            self.advance()
        } else {
            Err(SyntaxError::new(
                format!("Expected '{}', found {:?}", kw.as_str(), self.cur.kind),
                self.cur.span,
            ))
        }
    }

    fn is_ident_word(&self, word: &str) -> bool {
        matches!(
            &self.cur.kind,
            TokenKind::Ident { name, escaped: false } if name == word
        )
    }

    fn eat(&mut self, kind: TokenKind) -> Result<bool, SyntaxError> {
        if self.cur.kind == kind {
            self.advance()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Consumes `;` or applies Automatic Semicolon Insertion (ASI).
    fn consume_semicolon(&mut self) -> Result<(), SyntaxError> {
        if self.cur.kind == TokenKind::Semicolon {
            self.advance()?;
            Ok(())
        } else if self.cur.line_terminator_before
            || matches!(self.cur.kind, TokenKind::RBrace | TokenKind::Eof)
        {
            Ok(())
        } else {
            Err(SyntaxError::new(
                format!("Expected ';' before {:?}", self.cur.kind),
                self.cur.span,
            ))
        }
    }

    fn validate_binding_ident(&self, name: &str, span: Span) -> Result<(), SyntaxError> {
        if self.ctx.strict {
            if name == "eval" || name == "arguments" {
                return Err(SyntaxError::new(
                    format!("Cannot bind '{name}' in strict mode"),
                    span,
                ));
            }
            if is_strict_reserved_word(name) {
                return Err(SyntaxError::new(
                    format!("Unexpected strict mode reserved word '{name}'"),
                    span,
                ));
            }
        }
        if name == "await" && (self.ctx.in_async || self.source_type == SourceType::Module) {
            return Err(SyntaxError::new(
                "'await' cannot be used as a binding identifier here",
                span,
            ));
        }
        if name == "yield" && (self.ctx.in_generator || self.ctx.strict) {
            return Err(SyntaxError::new(
                "'yield' cannot be used as a binding identifier here",
                span,
            ));
        }
        Ok(())
    }

    fn validate_assign_ident(&self, name: &str, span: Span) -> Result<(), SyntaxError> {
        if self.ctx.strict && (name == "eval" || name == "arguments") {
            return Err(SyntaxError::new(
                format!("Cannot assign to '{name}' in strict mode"),
                span,
            ));
        }
        Ok(())
    }

    fn register_lex_binding(&mut self, name: &str, span: Span) -> Result<(), SyntaxError> {
        self.validate_binding_ident(name, span)?;
        if let Some(scope) = self.ctx.lex_scopes.last_mut() {
            if !scope.insert(name.to_owned()) {
                return Err(SyntaxError::new(
                    format!("Identifier '{name}' has already been declared"),
                    span,
                ));
            }
        }
        Ok(())
    }

    fn register_var_binding(&mut self, name: &str, span: Span) -> Result<(), SyntaxError> {
        self.validate_binding_ident(name, span)?;
        if let Some(scope) = self.ctx.var_scopes.last_mut() {
            scope.insert(name.to_owned());
        }
        Ok(())
    }

    fn parse_program(&mut self) -> Result<Program, SyntaxError> {
        let start = self.cur.span.start;
        let (body, strict_directive) = self.parse_statement_list(TokenKind::Eof, true)?;
        let end = self.cur.span.end;
        Ok(Program {
            source_type: self.source_type,
            strict: self.ctx.strict || strict_directive,
            body,
            span: Span::new(start, end),
        })
    }

    /// Parses statements until `stop` token (`Eof` or `RBrace`), handling directive prologues.
    fn parse_statement_list(
        &mut self,
        stop: TokenKind,
        top_level: bool,
    ) -> Result<(Vec<Statement>, bool), SyntaxError> {
        let mut stmts = Vec::new();
        let mut in_prologue = true;
        let mut saw_use_strict = false;
        let saved_strict = self.ctx.strict;

        while self.cur.kind != stop && self.cur.kind != TokenKind::Eof {
            // Check directive prologue before parsing statement.
            if in_prologue {
                if let TokenKind::String {
                    ref value,
                    has_escape: false,
                    ..
                } = self.cur.kind
                {
                    let is_use_strict = value == "use strict";
                    let stmt = self.parse_statement_or_module_item(top_level)?;
                    if matches!(
                        stmt,
                        Statement::Expr {
                            expr: Expr {
                                kind: ExprKind::String(_),
                                ..
                            },
                            ..
                        }
                    ) {
                        if is_use_strict {
                            saw_use_strict = true;
                            self.ctx.strict = true;
                        }
                        stmts.push(stmt);
                        continue;
                    } else {
                        in_prologue = false;
                        stmts.push(stmt);
                        continue;
                    }
                } else {
                    in_prologue = false;
                }
            }
            stmts.push(self.parse_statement_or_module_item(top_level)?);
        }

        if !top_level {
            self.ctx.strict = saved_strict;
        }
        Ok((stmts, saw_use_strict))
    }

    fn parse_statement_or_module_item(
        &mut self,
        top_level: bool,
    ) -> Result<Statement, SyntaxError> {
        match self.cur.kind {
            TokenKind::Keyword(Keyword::Import) => {
                // Distinguish dynamic `import(...)` and `import.meta` from `import` declaration.
                let next_kind = self.peek_token()?.kind.clone();
                if matches!(next_kind, TokenKind::LParen | TokenKind::Dot) {
                    return self.parse_expr_statement();
                }
                if self.source_type != SourceType::Module || !top_level {
                    return Err(SyntaxError::new(
                        "'import' declarations may only appear at top level of a module",
                        self.cur.span,
                    ));
                }
                self.parse_import_decl().map(Statement::ImportDecl)
            }
            TokenKind::Keyword(Keyword::Export) => {
                if self.source_type != SourceType::Module || !top_level {
                    return Err(SyntaxError::new(
                        "'export' declarations may only appear at top level of a module",
                        self.cur.span,
                    ));
                }
                self.parse_export_decl().map(Statement::ExportDecl)
            }
            _ => self.parse_statement(),
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, SyntaxError> {
        self.with_depth(Self::parse_statement_inner)
    }

    fn parse_statement_inner(&mut self) -> Result<Statement, SyntaxError> {
        if self.is_ident_word("let") && self.is_let_declaration()? {
            return self.parse_var_decl_stmt(VarKind::Let);
        }
        if self.is_ident_word("async") && self.is_async_function_ahead()? {
            let func = self.parse_function(true, true)?;
            if let Some(ref name) = func.name {
                self.register_var_binding(name, func.span)?;
            }
            return Ok(Statement::FunctionDecl(func));
        }
        match self.cur.kind {
            TokenKind::LBrace => self.parse_block_statement(),
            TokenKind::Semicolon => {
                let tok = self.advance()?;
                Ok(Statement::Empty { span: tok.span })
            }
            TokenKind::Keyword(Keyword::Var) => self.parse_var_decl_stmt(VarKind::Var),
            TokenKind::Keyword(Keyword::Const) => self.parse_var_decl_stmt(VarKind::Const),
            TokenKind::Keyword(Keyword::If) => self.parse_if_stmt(),
            TokenKind::Keyword(Keyword::While) => self.parse_while_stmt(),
            TokenKind::Keyword(Keyword::Do) => self.parse_do_while_stmt(),
            TokenKind::Keyword(Keyword::For) => self.parse_for_stmt(),
            TokenKind::Keyword(Keyword::Switch) => self.parse_switch_stmt(),
            TokenKind::Keyword(Keyword::Break) => self.parse_break_stmt(),
            TokenKind::Keyword(Keyword::Continue) => self.parse_continue_stmt(),
            TokenKind::Keyword(Keyword::Return) => self.parse_return_stmt(),
            TokenKind::Keyword(Keyword::Throw) => self.parse_throw_stmt(),
            TokenKind::Keyword(Keyword::Try) => self.parse_try_stmt(),
            TokenKind::Keyword(Keyword::With) => self.parse_with_stmt(),
            TokenKind::Keyword(Keyword::Debugger) => {
                let tok = self.advance()?;
                self.consume_semicolon()?;
                Ok(Statement::Debugger { span: tok.span })
            }
            TokenKind::Keyword(Keyword::Function) => {
                let func = self.parse_function(false, true)?;
                if let Some(ref name) = func.name {
                    self.register_var_binding(name, func.span)?;
                }
                Ok(Statement::FunctionDecl(func))
            }
            TokenKind::Keyword(Keyword::Class) => {
                let cls = self.parse_class(true)?;
                if let Some(ref name) = cls.name {
                    self.register_lex_binding(name, cls.span)?;
                }
                Ok(Statement::ClassDecl(cls))
            }
            _ => {
                // Check for labeled statement: `ident : stmt`.
                if self.can_be_label_name() && self.peek_token()?.kind == TokenKind::Colon {
                    return self.parse_labeled_stmt();
                }
                self.parse_expr_statement()
            }
        }
    }

    fn is_let_declaration(&mut self) -> Result<bool, SyntaxError> {
        let next = self.peek_token()?;
        let is_decl = match &next.kind {
            TokenKind::LBracket | TokenKind::LBrace => true,
            TokenKind::Ident { .. } => !next.line_terminator_before,
            TokenKind::Keyword(Keyword::Await | Keyword::Yield) => !next.line_terminator_before,
            _ => false,
        };
        Ok(is_decl)
    }

    fn is_async_function_ahead(&mut self) -> Result<bool, SyntaxError> {
        let next = self.peek_token()?;
        Ok(!next.line_terminator_before && next.kind == TokenKind::Keyword(Keyword::Function))
    }

    fn can_be_label_name(&self) -> bool {
        match &self.cur.kind {
            TokenKind::Ident { .. } => true,
            TokenKind::Keyword(Keyword::Await) => {
                !self.ctx.in_async && self.source_type != SourceType::Module
            }
            TokenKind::Keyword(Keyword::Yield) => !self.ctx.in_generator && !self.ctx.strict,
            _ => false,
        }
    }

    fn parse_block_statement(&mut self) -> Result<Statement, SyntaxError> {
        let start = self.expect(TokenKind::LBrace)?.span.start;
        self.ctx.lex_scopes.push(HashSet::new());
        let mut body = Vec::new();
        while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
            body.push(self.parse_statement()?);
        }
        self.ctx.lex_scopes.pop();
        let end = self.expect(TokenKind::RBrace)?.span.end;
        Ok(Statement::Block {
            body,
            span: Span::new(start, end),
        })
    }

    fn parse_var_decl_stmt(&mut self, kind: VarKind) -> Result<Statement, SyntaxError> {
        let (decls, span) = self.parse_var_decl_inner(kind, true)?;
        self.consume_semicolon()?;
        Ok(Statement::VarDecl { kind, decls, span })
    }

    fn parse_var_decl_inner(
        &mut self,
        kind: VarKind,
        require_const_init: bool,
    ) -> Result<(Vec<VarDeclarator>, Span), SyntaxError> {
        let start = self.advance()?.span.start;
        let mut decls = Vec::new();
        loop {
            let id = self.parse_binding_pattern()?;
            let mut names = Vec::new();
            id.collect_bound_names(&mut names);
            for (name, span) in names {
                if kind == VarKind::Let || kind == VarKind::Const {
                    if name == "let" {
                        return Err(SyntaxError::new(
                            "'let' is disallowed as a lexically bound name",
                            span,
                        ));
                    }
                    self.register_lex_binding(&name, span)?;
                } else {
                    self.register_var_binding(&name, span)?;
                }
            }

            let init = if self.eat(TokenKind::Eq)? {
                Some(self.parse_assign_expr()?)
            } else {
                if kind == VarKind::Const && require_const_init {
                    return Err(SyntaxError::new(
                        "Missing initializer in const declaration",
                        id.span(),
                    ));
                }
                if !matches!(id, Pattern::Ident { .. }) && require_const_init {
                    return Err(SyntaxError::new(
                        "Missing initializer in destructuring declaration",
                        id.span(),
                    ));
                }
                None
            };
            let end = init.as_ref().map_or(id.span().end, |e| e.span.end);
            let decl_span = Span::new(id.span().start, end);
            decls.push(VarDeclarator {
                id,
                init,
                span: decl_span,
            });

            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        let end = decls.last().map_or(start, |d| d.span.end);
        Ok((decls, Span::new(start, end)))
    }

    fn parse_if_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let start = self.expect_keyword(Keyword::If)?.span.start;
        self.expect(TokenKind::LParen)?;
        let test = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        let consequent = Box::new(self.parse_statement()?);
        let alternate = if self.cur.kind == TokenKind::Keyword(Keyword::Else) {
            self.advance()?;
            Some(Box::new(self.parse_statement()?))
        } else {
            None
        };
        let end = alternate
            .as_ref()
            .map_or(consequent.span().end, |a| a.span().end);
        Ok(Statement::If {
            test,
            consequent,
            alternate,
            span: Span::new(start, end),
        })
    }

    fn parse_while_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let start = self.expect_keyword(Keyword::While)?.span.start;
        self.expect(TokenKind::LParen)?;
        let test = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        let prev_loop = self.ctx.in_loop;
        self.ctx.in_loop = true;
        let body = Box::new(self.parse_statement()?);
        self.ctx.in_loop = prev_loop;
        let end = body.span().end;
        Ok(Statement::While {
            test,
            body,
            span: Span::new(start, end),
        })
    }

    fn parse_do_while_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let start = self.expect_keyword(Keyword::Do)?.span.start;
        let prev_loop = self.ctx.in_loop;
        self.ctx.in_loop = true;
        let body = Box::new(self.parse_statement()?);
        self.ctx.in_loop = prev_loop;
        self.expect_keyword(Keyword::While)?;
        self.expect(TokenKind::LParen)?;
        let test = self.parse_expr()?;
        let end = self.expect(TokenKind::RParen)?.span.end;
        let _ = self.eat(TokenKind::Semicolon)?; // ES6+: ASI always succeeds after do-while ')'
        Ok(Statement::DoWhile {
            body,
            test,
            span: Span::new(start, end),
        })
    }

    fn parse_for_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let start = self.expect_keyword(Keyword::For)?.span.start;
        let is_await = if self.cur.kind == TokenKind::Keyword(Keyword::Await) {
            if !self.ctx.in_async {
                return Err(SyntaxError::new(
                    "'for await...of' is only valid inside async functions or modules",
                    self.cur.span,
                ));
            }
            self.advance()?;
            true
        } else {
            false
        };

        self.expect(TokenKind::LParen)?;
        self.ctx.lex_scopes.push(HashSet::new());
        let saved_allow_in = self.ctx.allow_in;
        self.ctx.allow_in = false;

        let stmt = if self.cur.kind == TokenKind::Semicolon {
            if is_await {
                return Err(SyntaxError::new(
                    "'for await' requires an 'of' loop",
                    self.cur.span,
                ));
            }
            self.ctx.allow_in = saved_allow_in;
            self.parse_for_classic_rest(start, None)?
        } else if matches!(
            self.cur.kind,
            TokenKind::Keyword(Keyword::Var | Keyword::Const)
        ) || (self.is_ident_word("let") && self.is_let_declaration()?)
        {
            let kind = match self.cur.kind {
                TokenKind::Keyword(Keyword::Var) => VarKind::Var,
                TokenKind::Keyword(Keyword::Const) => VarKind::Const,
                _ => VarKind::Let,
            };
            let (decls, decl_span) = self.parse_var_decl_inner(kind, false)?;
            self.ctx.allow_in = saved_allow_in;

            if self.cur.kind == TokenKind::Keyword(Keyword::In) || self.is_ident_word("of") {
                let is_of = self.is_ident_word("of");
                if is_await && !is_of {
                    return Err(SyntaxError::new(
                        "'for await' requires 'of', not 'in'",
                        self.cur.span,
                    ));
                }
                if decls.len() != 1 || decls[0].init.is_some() {
                    return Err(SyntaxError::new(
                        "Invalid left-hand side in for-in/for-of loop",
                        decl_span,
                    ));
                }
                let id = decls.into_iter().next().expect("len 1").id;
                let left = ForEachLeft::VarDecl {
                    kind,
                    id,
                    span: decl_span,
                };
                self.parse_for_each_rest(start, is_of, is_await, left)?
            } else {
                if is_await {
                    return Err(SyntaxError::new(
                        "'for await' requires an 'of' loop",
                        self.cur.span,
                    ));
                }
                for d in &decls {
                    if kind == VarKind::Const && d.init.is_none() {
                        return Err(SyntaxError::new(
                            "Missing initializer in const declaration",
                            d.span,
                        ));
                    }
                }
                self.parse_for_classic_rest(
                    start,
                    Some(ForInit::VarDecl {
                        kind,
                        decls,
                        span: decl_span,
                    }),
                )?
            }
        } else {
            let expr = self.parse_expr()?;
            self.ctx.allow_in = saved_allow_in;
            if self.cur.kind == TokenKind::Keyword(Keyword::In) || self.is_ident_word("of") {
                let is_of = self.is_ident_word("of");
                if is_await && !is_of {
                    return Err(SyntaxError::new(
                        "'for await' requires 'of', not 'in'",
                        self.cur.span,
                    ));
                }
                let target = self.expr_to_assign_target(expr)?;
                self.parse_for_each_rest(start, is_of, is_await, ForEachLeft::Target(target))?
            } else {
                if is_await {
                    return Err(SyntaxError::new(
                        "'for await' requires an 'of' loop",
                        self.cur.span,
                    ));
                }
                self.parse_for_classic_rest(start, Some(ForInit::Expr(expr)))?
            }
        };

        self.ctx.lex_scopes.pop();
        Ok(stmt)
    }

    fn parse_for_each_rest(
        &mut self,
        start: u32,
        is_of: bool,
        is_await: bool,
        left: ForEachLeft,
    ) -> Result<Statement, SyntaxError> {
        self.advance()?; // consume `in` or `of`
        let right = if is_of {
            self.parse_assign_expr()?
        } else {
            self.parse_expr()?
        };
        self.expect(TokenKind::RParen)?;
        let prev_loop = self.ctx.in_loop;
        self.ctx.in_loop = true;
        let body = Box::new(self.parse_statement()?);
        self.ctx.in_loop = prev_loop;
        let end = body.span().end;
        if is_of {
            Ok(Statement::ForOf {
                is_await,
                left,
                right,
                body,
                span: Span::new(start, end),
            })
        } else {
            Ok(Statement::ForIn {
                left,
                right,
                body,
                span: Span::new(start, end),
            })
        }
    }

    fn parse_for_classic_rest(
        &mut self,
        start: u32,
        init: Option<ForInit>,
    ) -> Result<Statement, SyntaxError> {
        self.expect(TokenKind::Semicolon)?;
        let test = if self.cur.kind != TokenKind::Semicolon {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(TokenKind::Semicolon)?;
        let update = if self.cur.kind != TokenKind::RParen {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(TokenKind::RParen)?;
        let prev_loop = self.ctx.in_loop;
        self.ctx.in_loop = true;
        let body = Box::new(self.parse_statement()?);
        self.ctx.in_loop = prev_loop;
        let end = body.span().end;
        Ok(Statement::For {
            init,
            test,
            update,
            body,
            span: Span::new(start, end),
        })
    }

    fn parse_switch_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let start = self.expect_keyword(Keyword::Switch)?.span.start;
        self.expect(TokenKind::LParen)?;
        let discriminant = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        self.expect(TokenKind::LBrace)?;

        let prev_switch = self.ctx.in_switch;
        self.ctx.in_switch = true;
        self.ctx.lex_scopes.push(HashSet::new());

        let mut cases = Vec::new();
        let mut saw_default = false;
        while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
            let case_start = self.cur.span.start;
            let test = if self.cur.kind == TokenKind::Keyword(Keyword::Case) {
                self.advance()?;
                let expr = self.parse_expr()?;
                self.expect(TokenKind::Colon)?;
                Some(expr)
            } else if self.cur.kind == TokenKind::Keyword(Keyword::Default) {
                if saw_default {
                    return Err(SyntaxError::new(
                        "Multiple 'default' clauses in switch statement",
                        self.cur.span,
                    ));
                }
                saw_default = true;
                self.advance()?;
                self.expect(TokenKind::Colon)?;
                None
            } else {
                return Err(SyntaxError::new(
                    "Expected 'case' or 'default' in switch statement",
                    self.cur.span,
                ));
            };

            let mut consequent = Vec::new();
            while !matches!(
                self.cur.kind,
                TokenKind::Keyword(Keyword::Case | Keyword::Default)
                    | TokenKind::RBrace
                    | TokenKind::Eof
            ) {
                consequent.push(self.parse_statement()?);
            }
            let case_end = consequent.last().map_or(case_start, |s| s.span().end);
            cases.push(SwitchCase {
                test,
                consequent,
                span: Span::new(case_start, case_end),
            });
        }

        self.ctx.lex_scopes.pop();
        self.ctx.in_switch = prev_switch;
        let end = self.expect(TokenKind::RBrace)?.span.end;
        Ok(Statement::Switch {
            discriminant,
            cases,
            span: Span::new(start, end),
        })
    }

    fn parse_break_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let tok = self.expect_keyword(Keyword::Break)?;
        let label = if !self.cur.line_terminator_before
            && !matches!(
                self.cur.kind,
                TokenKind::Semicolon | TokenKind::RBrace | TokenKind::Eof
            ) {
            let (name, span) = self.parse_ident_name()?;
            if !self.ctx.labels.iter().any(|(l, _)| l == &name) {
                return Err(SyntaxError::new(
                    format!("Undefined label '{name}' in break statement"),
                    span,
                ));
            }
            Some(name)
        } else {
            if !self.ctx.in_loop && !self.ctx.in_switch {
                return Err(SyntaxError::new(
                    "Illegal break statement outside loop or switch",
                    tok.span,
                ));
            }
            None
        };
        let end = self.cur.span.start;
        self.consume_semicolon()?;
        Ok(Statement::Break {
            label,
            span: Span::new(tok.span.start, end),
        })
    }

    fn parse_continue_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let tok = self.expect_keyword(Keyword::Continue)?;
        let label = if !self.cur.line_terminator_before
            && !matches!(
                self.cur.kind,
                TokenKind::Semicolon | TokenKind::RBrace | TokenKind::Eof
            ) {
            let (name, span) = self.parse_ident_name()?;
            if !self
                .ctx
                .labels
                .iter()
                .any(|(l, is_loop)| l == &name && *is_loop)
            {
                return Err(SyntaxError::new(
                    format!("Undefined or non-iteration label '{name}' in continue statement"),
                    span,
                ));
            }
            Some(name)
        } else {
            if !self.ctx.in_loop {
                return Err(SyntaxError::new(
                    "Illegal continue statement outside iteration statement",
                    tok.span,
                ));
            }
            None
        };
        let end = self.cur.span.start;
        self.consume_semicolon()?;
        Ok(Statement::Continue {
            label,
            span: Span::new(tok.span.start, end),
        })
    }

    fn parse_return_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let tok = self.expect_keyword(Keyword::Return)?;
        if !self.ctx.in_function {
            return Err(SyntaxError::new(
                "Illegal return statement outside function",
                tok.span,
            ));
        }
        let arg = if self.cur.line_terminator_before
            || matches!(
                self.cur.kind,
                TokenKind::Semicolon | TokenKind::RBrace | TokenKind::Eof
            ) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        let end = arg.as_ref().map_or(tok.span.end, |e| e.span.end);
        self.consume_semicolon()?;
        Ok(Statement::Return {
            arg,
            span: Span::new(tok.span.start, end),
        })
    }

    fn parse_throw_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let tok = self.expect_keyword(Keyword::Throw)?;
        if self.cur.line_terminator_before {
            return Err(SyntaxError::new(
                "Illegal newline after throw",
                self.cur.span,
            ));
        }
        let arg = self.parse_expr()?;
        let end = arg.span.end;
        self.consume_semicolon()?;
        Ok(Statement::Throw {
            arg,
            span: Span::new(tok.span.start, end),
        })
    }

    fn parse_try_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let start = self.expect_keyword(Keyword::Try)?.span.start;
        let block = self.parse_block_body()?;

        let handler = if self.cur.kind == TokenKind::Keyword(Keyword::Catch) {
            let c_start = self.advance()?.span.start;
            self.ctx.lex_scopes.push(HashSet::new());
            let param = if self.eat(TokenKind::LParen)? {
                let pat = self.parse_binding_pattern()?;
                let mut names = Vec::new();
                pat.collect_bound_names(&mut names);
                for (name, span) in names {
                    self.register_lex_binding(&name, span)?;
                }
                self.expect(TokenKind::RParen)?;
                Some(pat)
            } else {
                None
            };
            let body = self.parse_block_body_in_current_scope()?;
            self.ctx.lex_scopes.pop();
            let c_end = self.cur.span.start;
            Some(CatchClause {
                param,
                body,
                span: Span::new(c_start, c_end),
            })
        } else {
            None
        };

        let finalizer = if self.cur.kind == TokenKind::Keyword(Keyword::Finally) {
            self.advance()?;
            Some(self.parse_block_body()?)
        } else {
            None
        };

        if handler.is_none() && finalizer.is_none() {
            return Err(SyntaxError::new(
                "Missing catch or finally after try",
                Span::new(start, self.cur.span.end),
            ));
        }
        let end = self.cur.span.start;
        Ok(Statement::Try {
            block,
            handler,
            finalizer,
            span: Span::new(start, end),
        })
    }

    fn parse_block_body(&mut self) -> Result<Vec<Statement>, SyntaxError> {
        self.ctx.lex_scopes.push(HashSet::new());
        let body = self.parse_block_body_in_current_scope()?;
        self.ctx.lex_scopes.pop();
        Ok(body)
    }

    fn parse_block_body_in_current_scope(&mut self) -> Result<Vec<Statement>, SyntaxError> {
        self.expect(TokenKind::LBrace)?;
        let mut body = Vec::new();
        while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
            body.push(self.parse_statement()?);
        }
        self.expect(TokenKind::RBrace)?;
        Ok(body)
    }

    fn parse_with_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let tok = self.expect_keyword(Keyword::With)?;
        if self.ctx.strict {
            return Err(SyntaxError::new(
                "Strict mode code may not include a with statement",
                tok.span,
            ));
        }
        self.expect(TokenKind::LParen)?;
        let object = self.parse_expr()?;
        self.expect(TokenKind::RParen)?;
        let body = Box::new(self.parse_statement()?);
        let end = body.span().end;
        Ok(Statement::With {
            object,
            body,
            span: Span::new(tok.span.start, end),
        })
    }

    fn parse_labeled_stmt(&mut self) -> Result<Statement, SyntaxError> {
        let (label, span) = self.parse_ident_name()?;
        self.expect(TokenKind::Colon)?;
        if self.ctx.labels.iter().any(|(l, _)| l == &label) {
            return Err(SyntaxError::new(
                format!("Label '{label}' has already been declared"),
                span,
            ));
        }
        let is_loop = matches!(
            self.cur.kind,
            TokenKind::Keyword(Keyword::For | Keyword::While | Keyword::Do)
        );
        self.ctx.labels.push((label.clone(), is_loop));
        let body = Box::new(self.parse_statement()?);
        self.ctx.labels.pop();
        let end = body.span().end;
        Ok(Statement::Labeled {
            label,
            body,
            span: Span::new(span.start, end),
        })
    }

    fn parse_expr_statement(&mut self) -> Result<Statement, SyntaxError> {
        let expr = self.parse_expr()?;
        let span = expr.span;
        self.consume_semicolon()?;
        Ok(Statement::Expr { expr, span })
    }

    // -----------------------------------------------------------------------
    // ES Modules (`import` / `export`)
    // -----------------------------------------------------------------------

    fn parse_optional_import_attributes(&mut self) -> Result<Option<String>, SyntaxError> {
        if self.cur.line_terminator_before || self.cur.kind != TokenKind::Keyword(Keyword::With) {
            return Ok(None);
        }
        self.advance()?;
        self.expect(TokenKind::LBrace)?;
        let mut import_type = None;
        let mut seen_keys = HashSet::new();
        while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
            let key = match &self.cur.kind {
                TokenKind::String { value, .. } => {
                    let s = value.clone();
                    self.advance()?;
                    s
                }
                _ => self.parse_module_export_name()?,
            };
            if !seen_keys.insert(key.clone()) {
                return Err(SyntaxError::new(
                    format!("Duplicate import attribute '{key}'"),
                    self.cur.span,
                ));
            }
            self.expect(TokenKind::Colon)?;
            let (val, _) = self.expect_string()?;
            if key == "type" {
                import_type = Some(val);
            }
            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(import_type)
    }

    fn parse_import_decl(&mut self) -> Result<ImportDecl, SyntaxError> {
        let start = self.expect_keyword(Keyword::Import)?.span.start;
        let mut specifiers = Vec::new();

        // Side-effect import: `import "mod";`
        if let TokenKind::String { .. } = self.cur.kind {
            let (source, end) = self.expect_string()?;
            let import_type = self.parse_optional_import_attributes()?;
            self.consume_semicolon()?;
            return Ok(ImportDecl {
                specifiers,
                source,
                import_type,
                span: Span::new(start, end),
            });
        }

        // Default import: `import Foo ...`
        if !matches!(self.cur.kind, TokenKind::Star | TokenKind::LBrace) {
            let (local, span) = self.parse_binding_ident_name()?;
            self.register_lex_binding(&local, span)?;
            specifiers.push(ImportSpecifier::Default(local));
            if self.eat(TokenKind::Comma)? {
                self.parse_import_clause_tail(&mut specifiers)?;
            }
        } else {
            self.parse_import_clause_tail(&mut specifiers)?;
        }

        self.expect_contextual("from")?;
        let (source, end) = self.expect_string()?;
        let import_type = self.parse_optional_import_attributes()?;
        self.consume_semicolon()?;
        Ok(ImportDecl {
            specifiers,
            source,
            import_type,
            span: Span::new(start, end),
        })
    }

    fn parse_import_clause_tail(
        &mut self,
        specifiers: &mut Vec<ImportSpecifier>,
    ) -> Result<(), SyntaxError> {
        if self.eat(TokenKind::Star)? {
            self.expect_contextual("as")?;
            let (local, span) = self.parse_binding_ident_name()?;
            self.register_lex_binding(&local, span)?;
            specifiers.push(ImportSpecifier::Namespace(local));
        } else {
            self.expect(TokenKind::LBrace)?;
            while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
                let imported = self.parse_module_export_name()?;
                let (local, span) = if self.is_ident_word("as") {
                    self.advance()?;
                    self.parse_binding_ident_name()?
                } else {
                    (imported.clone(), self.cur.span)
                };
                self.register_lex_binding(&local, span)?;
                specifiers.push(ImportSpecifier::Named { imported, local });
                if !self.eat(TokenKind::Comma)? {
                    break;
                }
            }
            self.expect(TokenKind::RBrace)?;
        }
        Ok(())
    }

    fn parse_export_decl(&mut self) -> Result<ExportDecl, SyntaxError> {
        let start = self.expect_keyword(Keyword::Export)?.span.start;

        // `export default ...`
        if self.cur.kind == TokenKind::Keyword(Keyword::Default) {
            self.advance()?;
            if self.cur.kind == TokenKind::Keyword(Keyword::Function)
                || (self.is_ident_word("async") && self.is_async_function_ahead()?)
            {
                let is_async = self.is_ident_word("async");
                let func = self.parse_function(is_async, false)?;
                let end = func.span.end;
                return Ok(ExportDecl {
                    kind: ExportKind::DefaultFunction(func),
                    span: Span::new(start, end),
                });
            }
            if self.cur.kind == TokenKind::Keyword(Keyword::Class) {
                let cls = self.parse_class(false)?;
                let end = cls.span.end;
                return Ok(ExportDecl {
                    kind: ExportKind::DefaultClass(cls),
                    span: Span::new(start, end),
                });
            }
            let expr = self.parse_assign_expr()?;
            let end = expr.span.end;
            self.consume_semicolon()?;
            return Ok(ExportDecl {
                kind: ExportKind::DefaultExpr(Box::new(expr)),
                span: Span::new(start, end),
            });
        }

        // `export * [as ns] from "mod";`
        if self.eat(TokenKind::Star)? {
            let exported = if self.is_ident_word("as") {
                self.advance()?;
                Some(self.parse_module_export_name()?)
            } else {
                None
            };
            self.expect_contextual("from")?;
            let (source, end) = self.expect_string()?;
            let _ = self.parse_optional_import_attributes()?;
            self.consume_semicolon()?;
            return Ok(ExportDecl {
                kind: ExportKind::All { exported, source },
                span: Span::new(start, end),
            });
        }

        // `export { a, b as c } [from "mod"];`
        if self.eat(TokenKind::LBrace)? {
            let mut specifiers = Vec::new();
            while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
                let local = self.parse_module_export_name()?;
                let exported = if self.is_ident_word("as") {
                    self.advance()?;
                    self.parse_module_export_name()?
                } else {
                    local.clone()
                };
                specifiers.push(ExportSpecifier { local, exported });
                if !self.eat(TokenKind::Comma)? {
                    break;
                }
            }
            let mut end = self.expect(TokenKind::RBrace)?.span.end;
            let source = if self.is_ident_word("from") {
                self.advance()?;
                let (s, s_end) = self.expect_string()?;
                let _ = self.parse_optional_import_attributes()?;
                end = s_end;
                Some(s)
            } else {
                None
            };
            self.consume_semicolon()?;
            return Ok(ExportDecl {
                kind: ExportKind::Named { specifiers, source },
                span: Span::new(start, end),
            });
        }

        // `export var/let/const/function/async function/class ...`
        let decl = if self.is_ident_word("let") {
            self.parse_var_decl_stmt(VarKind::Let)?
        } else if self.is_ident_word("async") && self.is_async_function_ahead()? {
            Statement::FunctionDecl(self.parse_function(true, true)?)
        } else {
            match self.cur.kind {
                TokenKind::Keyword(Keyword::Var) => self.parse_var_decl_stmt(VarKind::Var)?,
                TokenKind::Keyword(Keyword::Const) => self.parse_var_decl_stmt(VarKind::Const)?,
                TokenKind::Keyword(Keyword::Function) => {
                    Statement::FunctionDecl(self.parse_function(false, true)?)
                }
                TokenKind::Keyword(Keyword::Class) => Statement::ClassDecl(self.parse_class(true)?),
                _ => {
                    return Err(SyntaxError::new(
                        "Unexpected token after 'export'",
                        self.cur.span,
                    ));
                }
            }
        };
        let end = decl.span().end;
        Ok(ExportDecl {
            kind: ExportKind::Declaration(Box::new(decl)),
            span: Span::new(start, end),
        })
    }

    fn parse_module_export_name(&mut self) -> Result<String, SyntaxError> {
        match self.cur.kind.clone() {
            TokenKind::String { value, .. } => {
                self.advance()?;
                Ok(value)
            }
            TokenKind::Ident { name, .. } => {
                self.advance()?;
                Ok(name)
            }
            TokenKind::Keyword(kw) => {
                self.advance()?;
                Ok(kw.as_str().to_owned())
            }
            _ => Err(SyntaxError::new(
                "Expected module export identifier or string",
                self.cur.span,
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Functions & Classes
    // -----------------------------------------------------------------------

    fn parse_function(
        &mut self,
        is_async: bool,
        require_name: bool,
    ) -> Result<Function, SyntaxError> {
        let start = if is_async {
            self.advance()?.span.start // consume `async`
        } else {
            self.cur.span.start
        };
        self.expect_keyword(Keyword::Function)?;
        let is_generator = self.eat(TokenKind::Star)?;

        let name = if self.cur.kind != TokenKind::LParen {
            let (n, span) = self.parse_binding_ident_name()?;
            self.validate_binding_ident(&n, span)?;
            Some(n)
        } else if require_name {
            return Err(SyntaxError::new(
                "Function declaration requires a function name",
                self.cur.span,
            ));
        } else {
            None
        };

        self.parse_function_params_and_body(start, name, is_async, is_generator, false, false)
    }

    fn parse_function_params_and_body(
        &mut self,
        start: u32,
        name: Option<String>,
        is_async: bool,
        is_generator: bool,
        in_method: bool,
        in_derived_constructor: bool,
    ) -> Result<Function, SyntaxError> {
        let saved_ctx = self.ctx.clone();
        self.ctx.in_function = true;
        self.ctx.in_async = is_async;
        self.ctx.in_generator = is_generator;
        self.ctx.in_loop = false;
        self.ctx.in_switch = false;
        self.ctx.in_method = in_method;
        self.ctx.in_derived_constructor = in_derived_constructor;
        self.ctx.labels.clear();
        self.ctx.lex_scopes = vec![HashSet::new()];
        self.ctx.var_scopes = vec![HashSet::new()];

        let (params, rest_param, has_simple_params) = self.parse_formal_params()?;
        self.expect(TokenKind::LBrace)?;
        let (body, saw_use_strict) = self.parse_statement_list(TokenKind::RBrace, false)?;
        let end = self.expect(TokenKind::RBrace)?.span.end;

        let strict = self.ctx.strict || saw_use_strict;
        if saw_use_strict && !has_simple_params {
            self.ctx = saved_ctx;
            return Err(SyntaxError::new(
                "Illegal 'use strict' directive in function with non-simple parameter list",
                Span::new(start, end),
            ));
        }
        self.ctx.strict = strict;
        self.validate_param_names(
            &params,
            rest_param.as_deref(),
            strict || !has_simple_params || in_method,
        )?;

        self.ctx = saved_ctx;
        Ok(Function {
            name,
            params,
            rest_param,
            body,
            is_async,
            is_generator,
            strict,
            has_simple_params,
            span: Span::new(start, end),
        })
    }

    fn parse_formal_params(&mut self) -> Result<FormalParams, SyntaxError> {
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        let mut rest_param = None;
        let mut simple = true;

        while self.cur.kind != TokenKind::RParen && self.cur.kind != TokenKind::Eof {
            if self.eat(TokenKind::Ellipsis)? {
                simple = false;
                let rest = self.parse_binding_pattern()?;
                if self.cur.kind == TokenKind::Eq {
                    return Err(SyntaxError::new(
                        "Rest parameter may not have a default initializer",
                        self.cur.span,
                    ));
                }
                rest_param = Some(Box::new(rest));
                if self.cur.kind == TokenKind::Comma {
                    return Err(SyntaxError::new(
                        "Rest parameter must be last formal parameter",
                        self.cur.span,
                    ));
                }
                break;
            }
            let pat = self.parse_binding_element()?;
            if !matches!(pat, Pattern::Ident { .. }) {
                simple = false;
            }
            params.push(pat);
            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        self.expect(TokenKind::RParen)?;
        Ok((params, rest_param, simple))
    }

    fn validate_param_names(
        &self,
        params: &[Pattern],
        rest: Option<&Pattern>,
        disallow_dupes: bool,
    ) -> Result<(), SyntaxError> {
        let mut names = Vec::new();
        for p in params {
            p.collect_bound_names(&mut names);
        }
        if let Some(r) = rest {
            r.collect_bound_names(&mut names);
        }
        let mut seen = HashSet::new();
        for (name, span) in names {
            if disallow_dupes && !seen.insert(name.clone()) {
                return Err(SyntaxError::new(
                    format!("Duplicate parameter name '{name}' not allowed in this context"),
                    span,
                ));
            }
            if self.ctx.strict && (name == "eval" || name == "arguments") {
                return Err(SyntaxError::new(
                    format!("Cannot bind '{name}' as parameter in strict mode"),
                    span,
                ));
            }
        }
        Ok(())
    }

    fn parse_class(&mut self, require_name: bool) -> Result<Class, SyntaxError> {
        let start = self.expect_keyword(Keyword::Class)?.span.start;
        let saved_strict = self.ctx.strict;
        let saved_class = self.ctx.in_class_body;
        self.ctx.strict = true; // Class bodies are always strict mode.
        self.ctx.in_class_body = true;

        let name = if !matches!(
            self.cur.kind,
            TokenKind::Keyword(Keyword::Extends) | TokenKind::LBrace
        ) {
            let (n, span) = self.parse_binding_ident_name()?;
            self.validate_binding_ident(&n, span)?;
            Some(n)
        } else if require_name {
            self.ctx.strict = saved_strict;
            self.ctx.in_class_body = saved_class;
            return Err(SyntaxError::new(
                "Class declaration requires a class name",
                self.cur.span,
            ));
        } else {
            None
        };

        let super_class = if self.cur.kind == TokenKind::Keyword(Keyword::Extends) {
            self.advance()?;
            Some(Box::new(self.parse_left_hand_side_expr()?))
        } else {
            None
        };

        self.expect(TokenKind::LBrace)?;
        let mut elements = Vec::new();
        let mut has_constructor = false;

        while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
            if self.eat(TokenKind::Semicolon)? {
                continue;
            }
            let elem_start = self.cur.span.start;

            // Static initialization block: `static { ... }`
            if self.is_ident_word("static") && self.peek_token()?.kind == TokenKind::LBrace {
                self.advance()?;
                let saved_in_fn = self.ctx.in_function;
                let saved_method = self.ctx.in_method;
                self.ctx.in_function = false;
                self.ctx.in_method = true;
                let body = self.parse_block_body()?;
                self.ctx.in_function = saved_in_fn;
                self.ctx.in_method = saved_method;
                let elem_end = self.cur.span.start;
                elements.push(ClassElement::StaticBlock {
                    body,
                    span: Span::new(elem_start, elem_end),
                });
                continue;
            }

            let is_static = if self.is_ident_word("static")
                && !matches!(
                    self.peek_token()?.kind,
                    TokenKind::LParen | TokenKind::Eq | TokenKind::Semicolon | TokenKind::RBrace
                ) {
                self.advance()?;
                true
            } else {
                false
            };

            let mut is_async = false;
            let mut is_generator = false;
            let mut kind = MethodKind::Method;

            if self.is_ident_word("async")
                && !self.peek_token()?.line_terminator_before
                && !matches!(
                    self.peek_token()?.kind,
                    TokenKind::LParen | TokenKind::Eq | TokenKind::Semicolon | TokenKind::RBrace
                )
            {
                self.advance()?;
                is_async = true;
            }
            if self.eat(TokenKind::Star)? {
                is_generator = true;
            } else if !is_async
                && (self.is_ident_word("get") || self.is_ident_word("set"))
                && !matches!(
                    self.peek_token()?.kind,
                    TokenKind::LParen | TokenKind::Eq | TokenKind::Semicolon | TokenKind::RBrace
                )
            {
                kind = if self.is_ident_word("get") {
                    MethodKind::Get
                } else {
                    MethodKind::Set
                };
                self.advance()?;
            }

            let key = self.parse_prop_key(true)?;

            if self.cur.kind == TokenKind::LParen {
                if !is_static
                    && matches!(&key, PropKey::Ident(s) | PropKey::String(s) if s == "constructor")
                {
                    if is_async || is_generator || kind != MethodKind::Method {
                        return Err(SyntaxError::new(
                            "Class constructor may not be async, generator, getter, or setter",
                            Span::new(elem_start, self.cur.span.end),
                        ));
                    }
                    if has_constructor {
                        return Err(SyntaxError::new(
                            "A class may only have one constructor",
                            Span::new(elem_start, self.cur.span.end),
                        ));
                    }
                    has_constructor = true;
                    kind = MethodKind::Constructor;
                }
                let is_derived_ctor = kind == MethodKind::Constructor && super_class.is_some();
                let func = self.parse_function_params_and_body(
                    elem_start,
                    None,
                    is_async,
                    is_generator,
                    true,
                    is_derived_ctor,
                )?;
                let elem_end = func.span.end;
                elements.push(ClassElement::Method {
                    key,
                    kind,
                    is_static,
                    function: func,
                    span: Span::new(elem_start, elem_end),
                });
            } else {
                if is_async || is_generator || kind != MethodKind::Method {
                    return Err(SyntaxError::new(
                        "Invalid class field modifier",
                        Span::new(elem_start, self.cur.span.end),
                    ));
                }
                let value = if self.eat(TokenKind::Eq)? {
                    let prev_method = self.ctx.in_method;
                    self.ctx.in_method = true;
                    let v = self.parse_assign_expr()?;
                    self.ctx.in_method = prev_method;
                    Some(v)
                } else {
                    None
                };
                let elem_end = value.as_ref().map_or(self.cur.span.start, |e| e.span.end);
                self.consume_semicolon()?;
                elements.push(ClassElement::Field {
                    key,
                    is_static,
                    value,
                    span: Span::new(elem_start, elem_end),
                });
            }
        }

        let end = self.expect(TokenKind::RBrace)?.span.end;
        self.ctx.strict = saved_strict;
        self.ctx.in_class_body = saved_class;
        Ok(Class {
            name,
            super_class,
            elements,
            span: Span::new(start, end),
        })
    }

    // -----------------------------------------------------------------------
    // Patterns (Destructuring)
    // -----------------------------------------------------------------------

    fn parse_binding_element(&mut self) -> Result<Pattern, SyntaxError> {
        let pat = self.parse_binding_pattern()?;
        if self.eat(TokenKind::Eq)? {
            let right = self.parse_assign_expr()?;
            let span = pat.span().merge(right.span);
            Ok(Pattern::Assign {
                left: Box::new(pat),
                right: Box::new(right),
                span,
            })
        } else {
            Ok(pat)
        }
    }

    fn parse_binding_pattern(&mut self) -> Result<Pattern, SyntaxError> {
        match &self.cur.kind {
            TokenKind::LBracket => self.parse_array_binding_pattern(),
            TokenKind::LBrace => self.parse_object_binding_pattern(),
            _ => {
                let (name, span) = self.parse_binding_ident_name()?;
                self.validate_binding_ident(&name, span)?;
                Ok(Pattern::Ident { name, span })
            }
        }
    }

    fn parse_array_binding_pattern(&mut self) -> Result<Pattern, SyntaxError> {
        let start = self.expect(TokenKind::LBracket)?.span.start;
        let mut elements = Vec::new();
        let mut rest = None;
        while self.cur.kind != TokenKind::RBracket && self.cur.kind != TokenKind::Eof {
            if self.eat(TokenKind::Comma)? {
                elements.push(None);
                continue;
            }
            if self.eat(TokenKind::Ellipsis)? {
                let r = self.parse_binding_pattern()?;
                if self.cur.kind == TokenKind::Comma {
                    return Err(SyntaxError::new(
                        "Rest element must be last element in array pattern",
                        self.cur.span,
                    ));
                }
                rest = Some(Box::new(r));
                break;
            }
            elements.push(Some(self.parse_binding_element()?));
            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        let end = self.expect(TokenKind::RBracket)?.span.end;
        Ok(Pattern::Array {
            elements,
            rest,
            span: Span::new(start, end),
        })
    }

    fn parse_object_binding_pattern(&mut self) -> Result<Pattern, SyntaxError> {
        let start = self.expect(TokenKind::LBrace)?.span.start;
        let mut props = Vec::new();
        let mut rest = None;

        while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
            if self.eat(TokenKind::Ellipsis)? {
                let r = self.parse_binding_pattern()?;
                if !matches!(r, Pattern::Ident { .. }) {
                    return Err(SyntaxError::new(
                        "Object rest element must be an identifier",
                        r.span(),
                    ));
                }
                if self.cur.kind == TokenKind::Comma {
                    return Err(SyntaxError::new(
                        "Rest element must be last element in object pattern",
                        self.cur.span,
                    ));
                }
                rest = Some(Box::new(r));
                break;
            }

            let prop_start = self.cur.span.start;
            let key = self.parse_prop_key(false)?;
            let (value, shorthand) = if self.eat(TokenKind::Colon)? {
                (self.parse_binding_element()?, false)
            } else {
                let PropKey::Ident(ref name) = key else {
                    return Err(SyntaxError::new(
                        "Expected ':' after non-identifier object pattern key",
                        self.cur.span,
                    ));
                };
                self.validate_binding_ident(name, Span::new(prop_start, self.cur.span.start))?;
                let ident_pat = Pattern::Ident {
                    name: name.clone(),
                    span: Span::new(prop_start, self.cur.span.start),
                };
                let pat = if self.eat(TokenKind::Eq)? {
                    let right = self.parse_assign_expr()?;
                    let span = ident_pat.span().merge(right.span);
                    Pattern::Assign {
                        left: Box::new(ident_pat),
                        right: Box::new(right),
                        span,
                    }
                } else {
                    ident_pat
                };
                (pat, true)
            };
            let prop_end = value.span().end;
            props.push(ObjectPatProp {
                key,
                value,
                shorthand,
                span: Span::new(prop_start, prop_end),
            });
            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        let end = self.expect(TokenKind::RBrace)?.span.end;
        Ok(Pattern::Object {
            props,
            rest,
            span: Span::new(start, end),
        })
    }

    // -----------------------------------------------------------------------
    // Expressions (Pratt Precedence Climbing)
    // -----------------------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr, SyntaxError> {
        let first = self.parse_assign_expr()?;
        if self.cur.kind != TokenKind::Comma {
            return Ok(first);
        }
        let start = first.span.start;
        let mut exprs = vec![first];
        while self.eat(TokenKind::Comma)? {
            exprs.push(self.parse_assign_expr()?);
        }
        let end = exprs.last().map_or(start, |e| e.span.end);
        Ok(Expr::new(ExprKind::Sequence(exprs), Span::new(start, end)))
    }

    fn parse_assign_expr(&mut self) -> Result<Expr, SyntaxError> {
        self.with_depth(Self::parse_assign_expr_inner)
    }

    fn parse_assign_expr_inner(&mut self) -> Result<Expr, SyntaxError> {
        // `yield` expression inside generators.
        if self.cur.kind == TokenKind::Keyword(Keyword::Yield) && self.ctx.in_generator {
            let start = self.advance()?.span.start;
            if self.cur.line_terminator_before
                || matches!(
                    self.cur.kind,
                    TokenKind::Semicolon
                        | TokenKind::RBrace
                        | TokenKind::RParen
                        | TokenKind::RBracket
                        | TokenKind::Comma
                        | TokenKind::Colon
                        | TokenKind::Eof
                )
            {
                return Ok(Expr::new(
                    ExprKind::Yield {
                        arg: None,
                        delegate: false,
                    },
                    Span::new(start, self.cur.span.start),
                ));
            }
            let delegate = self.eat(TokenKind::Star)?;
            let arg = self.parse_assign_expr()?;
            let end = arg.span.end;
            return Ok(Expr::new(
                ExprKind::Yield {
                    arg: Some(Box::new(arg)),
                    delegate,
                },
                Span::new(start, end),
            ));
        }

        // Check for arrow function (`x => ...`, `async x => ...`, `(...) => ...`, `async (...) => ...`).
        if let Some(arrow) = self.try_parse_arrow_function()? {
            return Ok(arrow);
        }

        let left = self.parse_conditional_expr()?;
        if let Some(op) = token_to_assign_op(&self.cur.kind) {
            self.advance()?;
            let target = self.expr_to_assign_target(left)?;
            if op != AssignOp::Assign && matches!(target, AssignTarget::Pattern(_)) {
                return Err(SyntaxError::new(
                    "Invalid destructuring assignment target with compound operator",
                    target.span(),
                ));
            }
            let right = self.parse_assign_expr()?;
            let span = target.span().merge(right.span);
            return Ok(Expr::new(
                ExprKind::Assign {
                    op,
                    left: target,
                    right: Box::new(right),
                },
                span,
            ));
        }
        Ok(left)
    }

    /// Deterministically checks for and parses an ES2024 arrow function without exponential backtracking.
    fn try_parse_arrow_function(&mut self) -> Result<Option<Expr>, SyntaxError> {
        let cp = self.checkpoint();
        let start = self.cur.span.start;
        let is_async = if self.is_ident_word("async") && !self.peek_token()?.line_terminator_before
        {
            matches!(
                self.peek_token()?.kind,
                TokenKind::LParen | TokenKind::Ident { .. } | TokenKind::Keyword(Keyword::Yield)
            )
        } else {
            false
        };

        if is_async {
            self.advance()?; // consume `async`
        }

        // Single identifier parameter: `[async] ident => ...`
        if matches!(
            self.cur.kind,
            TokenKind::Ident { .. } | TokenKind::Keyword(Keyword::Yield | Keyword::Await)
        ) {
            let next = self.peek_token()?;
            if next.kind == TokenKind::Arrow && !next.line_terminator_before {
                let (name, span) = self.parse_binding_ident_name()?;
                self.validate_binding_ident(&name, span)?;
                self.expect(TokenKind::Arrow)?;
                let param = Pattern::Ident { name, span };
                return self
                    .parse_arrow_body(start, vec![param], None, is_async, true)
                    .map(Some);
            }
            self.restore(cp);
            return Ok(None);
        }

        // Parenthesized parameter list: `[async] (...) => ...`
        if self.cur.kind == TokenKind::LParen && self.paren_followed_by_arrow()? {
            let saved_async = self.ctx.in_async;
            self.ctx.in_async = is_async;
            let parsed_params = self.parse_formal_params();
            self.ctx.in_async = saved_async;
            match parsed_params {
                Ok((params, rest_param, simple))
                    if self.cur.kind == TokenKind::Arrow && !self.cur.line_terminator_before =>
                {
                    self.advance()?; // consume `=>`
                    return self
                        .parse_arrow_body(start, params, rest_param, is_async, simple)
                        .map(Some);
                }
                Err(e) if is_async => return Err(e),
                _ => {
                    self.restore(cp);
                    return Ok(None);
                }
            }
        }

        self.restore(cp);
        Ok(None)
    }

    /// Scans matching `(...)` in O(tokens inside parens) to test if `)` is directly followed by `=>`.
    /// Rejects non-parameter starts (like IIFEs `(function(){...})()`) in O(1) and never propagates
    /// speculative lex errors (e.g. from regex/template literals inside non-arrow parenthesized exprs).
    fn paren_followed_by_arrow(&mut self) -> Result<bool, SyntaxError> {
        let cp = self.checkpoint();
        // Fast O(1) rejection: an arrow parameter list after `(` can only start with `)`, `...`,
        // an identifier (`yield`/`await` included), `{`, or `[`.
        let first_ok = match self.peek_token() {
            Ok(tok) => matches!(
                tok.kind,
                TokenKind::RParen
                    | TokenKind::Ellipsis
                    | TokenKind::Ident { .. }
                    | TokenKind::LBrace
                    | TokenKind::LBracket
                    | TokenKind::Keyword(Keyword::Yield | Keyword::Await)
            ),
            Err(_) => false,
        };
        if !first_ok {
            self.restore(cp);
            return Ok(false);
        }

        let mut paren_depth = 0u32;
        let mut brace_depth = 0u32;
        let mut tpl_brace_stack: Vec<u32> = Vec::new();
        let result = loop {
            match self.cur.kind {
                TokenKind::LParen => paren_depth += 1,
                TokenKind::RParen => {
                    paren_depth = paren_depth.saturating_sub(1);
                    if paren_depth == 0 {
                        let is_arrow = match self.peek_token() {
                            Ok(next) => {
                                next.kind == TokenKind::Arrow && !next.line_terminator_before
                            }
                            Err(_) => false,
                        };
                        break is_arrow;
                    }
                }
                TokenKind::TemplateHead { .. } => {
                    tpl_brace_stack.push(brace_depth);
                    brace_depth += 1;
                }
                TokenKind::LBrace => brace_depth += 1,
                TokenKind::RBrace => {
                    brace_depth = brace_depth.saturating_sub(1);
                    if tpl_brace_stack.last().copied() == Some(brace_depth) {
                        let rbrace_end = self.cur.span.end as usize;
                        self.peek = None;
                        match self.lexer.lex_template_continuation(rbrace_end) {
                            Ok(cont) => match cont.kind {
                                TokenKind::TemplateMiddle { .. } => {
                                    brace_depth += 1;
                                }
                                TokenKind::TemplateTail { .. } => {
                                    tpl_brace_stack.pop();
                                }
                                _ => break false,
                            },
                            Err(_) => break false,
                        }
                        match self.lexer.next_token() {
                            Ok(tok) => {
                                self.cur = tok;
                                continue;
                            }
                            Err(_) => break false,
                        }
                    }
                }
                TokenKind::Eof => break false,
                _ => {}
            }
            if self.advance().is_err() {
                break false;
            }
        };
        self.restore(cp);
        Ok(result)
    }

    fn parse_arrow_body(
        &mut self,
        start: u32,
        params: Vec<Pattern>,
        rest_param: Option<Box<Pattern>>,
        is_async: bool,
        has_simple_params: bool,
    ) -> Result<Expr, SyntaxError> {
        let saved_ctx = self.ctx.clone();
        self.ctx.in_function = true;
        self.ctx.in_async = is_async;
        self.ctx.in_generator = false;
        self.ctx.in_loop = false;
        self.ctx.in_switch = false;
        self.ctx.labels.clear();
        self.ctx.lex_scopes = vec![HashSet::new()];
        self.ctx.var_scopes = vec![HashSet::new()];

        let (body, end, strict) = if self.cur.kind == TokenKind::LBrace {
            self.advance()?;
            let (stmts, saw_strict) = self.parse_statement_list(TokenKind::RBrace, false)?;
            let end = self.expect(TokenKind::RBrace)?.span.end;
            if saw_strict && !has_simple_params {
                self.ctx = saved_ctx;
                return Err(SyntaxError::new(
                    "Illegal 'use strict' directive in arrow function with non-simple parameters",
                    Span::new(start, end),
                ));
            }
            let s = self.ctx.strict || saw_strict;
            (
                ArrowBody::Block {
                    body: stmts,
                    strict: s,
                },
                end,
                s,
            )
        } else {
            let expr = self.parse_assign_expr()?;
            let end = expr.span.end;
            (ArrowBody::Expr(Box::new(expr)), end, self.ctx.strict)
        };

        // Arrow functions never permit duplicate parameter names.
        self.ctx.strict = strict;
        self.validate_param_names(&params, rest_param.as_deref(), true)?;
        self.ctx = saved_ctx;

        Ok(Expr::new(
            ExprKind::Arrow {
                params,
                rest_param,
                body,
                is_async,
                has_simple_params,
            },
            Span::new(start, end),
        ))
    }

    fn parse_conditional_expr(&mut self) -> Result<Expr, SyntaxError> {
        let test = self.parse_binary_expr(1)?;
        if !self.eat(TokenKind::Question)? {
            return Ok(test);
        }
        let saved_in = self.ctx.allow_in;
        self.ctx.allow_in = true;
        let consequent = self.parse_assign_expr()?;
        self.ctx.allow_in = saved_in;
        self.expect(TokenKind::Colon)?;
        let alternate = self.parse_assign_expr()?;
        let span = test.span.merge(alternate.span);
        Ok(Expr::new(
            ExprKind::Conditional {
                test: Box::new(test),
                consequent: Box::new(consequent),
                alternate: Box::new(alternate),
            },
            span,
        ))
    }

    /// Pratt expression parser for all logical and binary operators (precedence 1..=12).
    fn parse_binary_expr(&mut self, min_prec: u8) -> Result<Expr, SyntaxError> {
        let mut left = self.parse_unary_expr()?;
        while let Some((prec, right_assoc, op)) = self.peek_binary_op() {
            if prec < min_prec {
                break;
            }
            let op_span = self.cur.span;
            self.advance()?;

            // ES2020: forbid unparenthesized unary operand on LHS of `**`.
            if matches!(op, BinOrLogOp::Binary(BinaryOp::Exp))
                && matches!(left.kind, ExprKind::Unary { .. } | ExprKind::Await(_))
            {
                return Err(SyntaxError::new(
                    "Unary operator used immediately before exponentiation expression; wrap in parentheses",
                    op_span,
                ));
            }

            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.parse_binary_expr(next_min)?;
            let span = left.span.merge(right.span);

            left = match op {
                BinOrLogOp::Logical(log_op) => {
                    // ES2020 early error: cannot mix unparenthesized `??` with `&&` or `||`.
                    let mixes_nullish = match log_op {
                        LogicalOp::Nullish => is_bare_and_or(&left) || is_bare_and_or(&right),
                        LogicalOp::And | LogicalOp::Or => {
                            is_bare_nullish(&left) || is_bare_nullish(&right)
                        }
                    };
                    if mixes_nullish {
                        return Err(SyntaxError::new(
                            "Cannot mix '??' with '&&' or '||' without parentheses",
                            op_span,
                        ));
                    }
                    Expr::new(
                        ExprKind::Logical {
                            op: log_op,
                            left: Box::new(left),
                            right: Box::new(right),
                        },
                        span,
                    )
                }
                BinOrLogOp::Binary(bin_op) => Expr::new(
                    ExprKind::Binary {
                        op: bin_op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    span,
                ),
            };
        }
        Ok(left)
    }

    fn peek_binary_op(&self) -> Option<(u8, bool, BinOrLogOp)> {
        let op = match self.cur.kind {
            TokenKind::QuestionQuestion => (1, false, BinOrLogOp::Logical(LogicalOp::Nullish)),
            TokenKind::PipePipe => (2, false, BinOrLogOp::Logical(LogicalOp::Or)),
            TokenKind::AmpAmp => (3, false, BinOrLogOp::Logical(LogicalOp::And)),
            TokenKind::Pipe => (4, false, BinOrLogOp::Binary(BinaryOp::BitOr)),
            TokenKind::Caret => (5, false, BinOrLogOp::Binary(BinaryOp::BitXor)),
            TokenKind::Amp => (6, false, BinOrLogOp::Binary(BinaryOp::BitAnd)),
            TokenKind::EqEq => (7, false, BinOrLogOp::Binary(BinaryOp::Eq)),
            TokenKind::BangEq => (7, false, BinOrLogOp::Binary(BinaryOp::NotEq)),
            TokenKind::EqEqEq => (7, false, BinOrLogOp::Binary(BinaryOp::StrictEq)),
            TokenKind::BangEqEq => (7, false, BinOrLogOp::Binary(BinaryOp::StrictNotEq)),
            TokenKind::Lt => (8, false, BinOrLogOp::Binary(BinaryOp::Lt)),
            TokenKind::LtEq => (8, false, BinOrLogOp::Binary(BinaryOp::LtEq)),
            TokenKind::Gt => (8, false, BinOrLogOp::Binary(BinaryOp::Gt)),
            TokenKind::GtEq => (8, false, BinOrLogOp::Binary(BinaryOp::GtEq)),
            TokenKind::Keyword(Keyword::InstanceOf) => {
                (8, false, BinOrLogOp::Binary(BinaryOp::InstanceOf))
            }
            TokenKind::Keyword(Keyword::In) if self.ctx.allow_in => {
                (8, false, BinOrLogOp::Binary(BinaryOp::In))
            }
            TokenKind::LShift => (9, false, BinOrLogOp::Binary(BinaryOp::Shl)),
            TokenKind::RShift => (9, false, BinOrLogOp::Binary(BinaryOp::Shr)),
            TokenKind::URShift => (9, false, BinOrLogOp::Binary(BinaryOp::UShr)),
            TokenKind::Plus => (10, false, BinOrLogOp::Binary(BinaryOp::Add)),
            TokenKind::Minus => (10, false, BinOrLogOp::Binary(BinaryOp::Sub)),
            TokenKind::Star => (11, false, BinOrLogOp::Binary(BinaryOp::Mul)),
            TokenKind::Slash => (11, false, BinOrLogOp::Binary(BinaryOp::Div)),
            TokenKind::Percent => (11, false, BinOrLogOp::Binary(BinaryOp::Rem)),
            TokenKind::StarStar => (12, true, BinOrLogOp::Binary(BinaryOp::Exp)),
            _ => return None,
        };
        Some(op)
    }

    fn parse_unary_expr(&mut self) -> Result<Expr, SyntaxError> {
        let start = self.cur.span.start;
        if self.cur.kind == TokenKind::Keyword(Keyword::Await) && self.ctx.in_async {
            self.advance()?;
            let arg = self.parse_unary_expr()?;
            let span = Span::new(start, arg.span.end);
            return Ok(Expr::new(ExprKind::Await(Box::new(arg)), span));
        }

        let unary_op = match self.cur.kind {
            TokenKind::Plus => Some(UnaryOp::Plus),
            TokenKind::Minus => Some(UnaryOp::Minus),
            TokenKind::Bang => Some(UnaryOp::Not),
            TokenKind::Tilde => Some(UnaryOp::BitNot),
            TokenKind::Keyword(Keyword::TypeOf) => Some(UnaryOp::TypeOf),
            TokenKind::Keyword(Keyword::Void) => Some(UnaryOp::Void),
            TokenKind::Keyword(Keyword::Delete) => Some(UnaryOp::Delete),
            _ => None,
        };
        if let Some(op) = unary_op {
            self.advance()?;
            let arg = self.parse_unary_expr()?;
            if op == UnaryOp::Delete {
                if self.ctx.strict && matches!(arg.kind, ExprKind::Ident(_)) {
                    return Err(SyntaxError::new(
                        "Delete of an unqualified identifier in strict mode",
                        arg.span,
                    ));
                }
                if matches!(
                    arg.kind,
                    ExprKind::Member {
                        property: MemberProp::Private(_),
                        ..
                    }
                ) {
                    return Err(SyntaxError::new(
                        "Private fields cannot be deleted",
                        arg.span,
                    ));
                }
            }
            let span = Span::new(start, arg.span.end);
            return Ok(Expr::new(
                ExprKind::Unary {
                    op,
                    arg: Box::new(arg),
                },
                span,
            ));
        }

        // Prefix `++` / `--`.
        if matches!(self.cur.kind, TokenKind::PlusPlus | TokenKind::MinusMinus) {
            let op = if self.cur.kind == TokenKind::PlusPlus {
                UpdateOp::Inc
            } else {
                UpdateOp::Dec
            };
            self.advance()?;
            let arg = self.parse_unary_expr()?;
            self.validate_update_target(&arg)?;
            let span = Span::new(start, arg.span.end);
            return Ok(Expr::new(
                ExprKind::Update {
                    op,
                    arg: Box::new(arg),
                    prefix: true,
                },
                span,
            ));
        }

        let expr = self.parse_left_hand_side_expr()?;

        // Postfix `++` / `--` (no LineTerminator allowed before operator).
        if !self.cur.line_terminator_before
            && matches!(self.cur.kind, TokenKind::PlusPlus | TokenKind::MinusMinus)
        {
            let op = if self.cur.kind == TokenKind::PlusPlus {
                UpdateOp::Inc
            } else {
                UpdateOp::Dec
            };
            let op_tok = self.advance()?;
            self.validate_update_target(&expr)?;
            let span = expr.span.merge(op_tok.span);
            return Ok(Expr::new(
                ExprKind::Update {
                    op,
                    arg: Box::new(expr),
                    prefix: false,
                },
                span,
            ));
        }

        Ok(expr)
    }

    fn validate_update_target(&self, expr: &Expr) -> Result<(), SyntaxError> {
        match &expr.kind {
            ExprKind::Ident(name) => self.validate_assign_ident(name, expr.span),
            ExprKind::Member {
                optional: false, ..
            } => Ok(()),
            ExprKind::Paren(inner) => self.validate_update_target(inner),
            _ => Err(SyntaxError::new(
                "Invalid left-hand side expression in update operation",
                expr.span,
            )),
        }
    }

    fn parse_left_hand_side_expr(&mut self) -> Result<Expr, SyntaxError> {
        let base = if self.cur.kind == TokenKind::Keyword(Keyword::New) {
            self.parse_new_expr()?
        } else {
            self.parse_primary_expr()?
        };
        self.parse_postfix_chain(base, true)
    }

    fn parse_new_expr(&mut self) -> Result<Expr, SyntaxError> {
        let start = self.expect_keyword(Keyword::New)?.span.start;
        // `new.target`
        if self.eat(TokenKind::Dot)? {
            let (prop, prop_span) = self.parse_prop_ident()?;
            if prop != "target" {
                return Err(SyntaxError::new(
                    format!("Invalid meta property 'new.{prop}'"),
                    prop_span,
                ));
            }
            if !self.ctx.in_function && !self.ctx.in_method {
                return Err(SyntaxError::new(
                    "'new.target' expression is not allowed outside a function",
                    Span::new(start, prop_span.end),
                ));
            }
            return Ok(Expr::new(
                ExprKind::NewTarget,
                Span::new(start, prop_span.end),
            ));
        }

        let callee = if self.cur.kind == TokenKind::Keyword(Keyword::New) {
            self.parse_new_expr()?
        } else {
            let primary = self.parse_primary_expr()?;
            self.parse_postfix_chain(primary, false)?
        };
        let (args, end) = if self.cur.kind == TokenKind::LParen {
            self.parse_call_args()?
        } else {
            (Vec::new(), callee.span.end)
        };
        Ok(Expr::new(
            ExprKind::New {
                callee: Box::new(callee),
                args,
            },
            Span::new(start, end),
        ))
    }

    fn parse_postfix_chain(
        &mut self,
        mut expr: Expr,
        allow_call: bool,
    ) -> Result<Expr, SyntaxError> {
        let mut is_optional_chain = false;
        loop {
            match self.cur.kind {
                TokenKind::Dot => {
                    self.advance()?;
                    let (prop, prop_span) =
                        if let TokenKind::PrivateIdent(name) = self.cur.kind.clone() {
                            let tok = self.advance()?;
                            (MemberProp::Private(name), tok.span)
                        } else {
                            let (name, span) = self.parse_prop_ident()?;
                            (MemberProp::Ident(name), span)
                        };
                    let span = expr.span.merge(prop_span);
                    expr = Expr::new(
                        ExprKind::Member {
                            object: Box::new(expr),
                            property: prop,
                            optional: false,
                        },
                        span,
                    );
                }
                TokenKind::LBracket => {
                    self.advance()?;
                    let index = self.parse_expr()?;
                    let end = self.expect(TokenKind::RBracket)?.span.end;
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::Member {
                            object: Box::new(expr),
                            property: MemberProp::Computed(Box::new(index)),
                            optional: false,
                        },
                        span,
                    );
                }
                TokenKind::QuestionDot => {
                    if !allow_call {
                        return Err(SyntaxError::new(
                            "Optional chaining is not permitted in 'new' callee",
                            self.cur.span,
                        ));
                    }
                    is_optional_chain = true;
                    self.advance()?;
                    if self.cur.kind == TokenKind::LParen {
                        let (args, end) = self.parse_call_args()?;
                        let span = Span::new(expr.span.start, end);
                        expr = Expr::new(
                            ExprKind::Call {
                                callee: Box::new(expr),
                                args,
                                optional: true,
                            },
                            span,
                        );
                    } else if self.eat(TokenKind::LBracket)? {
                        let index = self.parse_expr()?;
                        let end = self.expect(TokenKind::RBracket)?.span.end;
                        let span = Span::new(expr.span.start, end);
                        expr = Expr::new(
                            ExprKind::Member {
                                object: Box::new(expr),
                                property: MemberProp::Computed(Box::new(index)),
                                optional: true,
                            },
                            span,
                        );
                    } else {
                        let (prop, prop_span) =
                            if let TokenKind::PrivateIdent(name) = self.cur.kind.clone() {
                                let tok = self.advance()?;
                                (MemberProp::Private(name), tok.span)
                            } else {
                                let (name, span) = self.parse_prop_ident()?;
                                (MemberProp::Ident(name), span)
                            };
                        let span = expr.span.merge(prop_span);
                        expr = Expr::new(
                            ExprKind::Member {
                                object: Box::new(expr),
                                property: prop,
                                optional: true,
                            },
                            span,
                        );
                    }
                }
                TokenKind::LParen if allow_call => {
                    if matches!(expr.kind, ExprKind::Super) && !self.ctx.in_derived_constructor {
                        return Err(SyntaxError::new(
                            "'super()' call is only valid in derived class constructors",
                            expr.span,
                        ));
                    }
                    let (args, end) = self.parse_call_args()?;
                    let span = Span::new(expr.span.start, end);
                    expr = Expr::new(
                        ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                            optional: false,
                        },
                        span,
                    );
                }
                TokenKind::TemplateNoSub { .. } | TokenKind::TemplateHead { .. } => {
                    if is_optional_chain {
                        return Err(SyntaxError::new(
                            "Tagged template literal cannot be used in optional chain",
                            self.cur.span,
                        ));
                    }
                    let (quasis, exprs, tpl_span) = self.parse_template_parts(true)?;
                    let span = expr.span.merge(tpl_span);
                    expr = Expr::new(
                        ExprKind::TaggedTemplate {
                            tag: Box::new(expr),
                            quasis,
                            exprs,
                        },
                        span,
                    );
                }
                _ => break,
            }
        }
        if is_optional_chain {
            let span = expr.span;
            Ok(Expr::new(ExprKind::Chain(Box::new(expr)), span))
        } else {
            Ok(expr)
        }
    }

    fn parse_call_args(&mut self) -> Result<(Vec<CallArg>, u32), SyntaxError> {
        self.expect(TokenKind::LParen)?;
        let mut args = Vec::new();
        while self.cur.kind != TokenKind::RParen && self.cur.kind != TokenKind::Eof {
            if self.eat(TokenKind::Ellipsis)? {
                args.push(CallArg::Spread(self.parse_assign_expr()?));
            } else {
                args.push(CallArg::Expr(self.parse_assign_expr()?));
            }
            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        let end = self.expect(TokenKind::RParen)?.span.end;
        Ok((args, end))
    }

    fn parse_primary_expr(&mut self) -> Result<Expr, SyntaxError> {
        match self.cur.kind.clone() {
            TokenKind::Keyword(Keyword::Null) => {
                let tok = self.advance()?;
                Ok(Expr::new(ExprKind::Null, tok.span))
            }
            TokenKind::Keyword(Keyword::True) => {
                let tok = self.advance()?;
                Ok(Expr::new(ExprKind::Bool(true), tok.span))
            }
            TokenKind::Keyword(Keyword::False) => {
                let tok = self.advance()?;
                Ok(Expr::new(ExprKind::Bool(false), tok.span))
            }
            TokenKind::Keyword(Keyword::This) => {
                let tok = self.advance()?;
                Ok(Expr::new(ExprKind::This, tok.span))
            }
            TokenKind::Keyword(Keyword::Super) => {
                let tok = self.advance()?;
                if !self.ctx.in_method && !self.ctx.in_derived_constructor {
                    return Err(SyntaxError::new(
                        "'super' keyword unexpected here",
                        tok.span,
                    ));
                }
                Ok(Expr::new(ExprKind::Super, tok.span))
            }
            TokenKind::Keyword(Keyword::Import) => {
                let start = self.advance()?.span.start;
                if self.eat(TokenKind::Dot)? {
                    let (prop, prop_span) = self.parse_prop_ident()?;
                    if prop != "meta" {
                        return Err(SyntaxError::new(
                            format!("Invalid meta property 'import.{prop}'"),
                            prop_span,
                        ));
                    }
                    if self.source_type != SourceType::Module {
                        return Err(SyntaxError::new(
                            "'import.meta' may only appear in a module",
                            Span::new(start, prop_span.end),
                        ));
                    }
                    return Ok(Expr::new(
                        ExprKind::ImportMeta,
                        Span::new(start, prop_span.end),
                    ));
                }
                self.expect(TokenKind::LParen)?;
                let source = self.parse_assign_expr()?;
                if self.eat(TokenKind::Comma)? && self.cur.kind != TokenKind::RParen {
                    let _options = self.parse_assign_expr()?;
                    let _ = self.eat(TokenKind::Comma)?;
                }
                let end = self.expect(TokenKind::RParen)?.span.end;
                Ok(Expr::new(
                    ExprKind::ImportCall {
                        source: Box::new(source),
                    },
                    Span::new(start, end),
                ))
            }
            TokenKind::Number {
                value,
                legacy_octal,
            } => {
                let tok = self.advance()?;
                if legacy_octal && self.ctx.strict {
                    return Err(SyntaxError::new(
                        "Octal literals are not allowed in strict mode",
                        tok.span,
                    ));
                }
                Ok(Expr::new(ExprKind::Number(value), tok.span))
            }
            TokenKind::BigInt(raw) => {
                let tok = self.advance()?;
                Ok(Expr::new(ExprKind::BigInt(raw), tok.span))
            }
            TokenKind::String {
                value,
                has_legacy_octal_escape,
                ..
            } => {
                let tok = self.advance()?;
                if has_legacy_octal_escape && self.ctx.strict {
                    return Err(SyntaxError::new(
                        "Octal escape sequences are not allowed in strict mode",
                        tok.span,
                    ));
                }
                Ok(Expr::new(ExprKind::String(value), tok.span))
            }
            // RegExp literal (disambiguated by expression-start context).
            TokenKind::Slash | TokenKind::SlashEq => {
                let slash_start = self.cur.span.start as usize;
                self.peek = None;
                let (pattern, flags, span) = self.lexer.lex_regex_literal(slash_start)?;
                self.cur = self.lexer.next_token()?;
                Ok(Expr::new(ExprKind::RegExp { pattern, flags }, span))
            }
            TokenKind::TemplateNoSub { .. } | TokenKind::TemplateHead { .. } => {
                let (quasis, exprs, span) = self.parse_template_parts(false)?;
                Ok(Expr::new(ExprKind::Template { quasis, exprs }, span))
            }
            TokenKind::PrivateIdent(name) => {
                let tok = self.advance()?;
                if !self.ctx.in_class_body {
                    return Err(SyntaxError::new(
                        format!("Private identifier '#{name}' cannot be used outside a class body"),
                        tok.span,
                    ));
                }
                Ok(Expr::new(ExprKind::PrivateIdent(name), tok.span))
            }
            TokenKind::Ident { name, escaped } => {
                if !escaped && name == "async" && self.is_async_function_ahead()? {
                    let func = self.parse_function(true, false)?;
                    let span = func.span;
                    return Ok(Expr::new(ExprKind::Function(Box::new(func)), span));
                }
                let tok = self.advance()?;
                if self.ctx.strict && is_strict_reserved_word(&name) {
                    return Err(SyntaxError::new(
                        format!("Unexpected strict mode reserved word '{name}'"),
                        tok.span,
                    ));
                }
                Ok(Expr::new(ExprKind::Ident(name), tok.span))
            }
            TokenKind::Keyword(Keyword::Await) => {
                let tok = self.advance()?;
                if self.ctx.in_async || self.source_type == SourceType::Module {
                    return Err(SyntaxError::new("Unexpected 'await' identifier", tok.span));
                }
                Ok(Expr::new(ExprKind::Ident("await".into()), tok.span))
            }
            TokenKind::Keyword(Keyword::Yield) => {
                let tok = self.advance()?;
                if self.ctx.in_generator || self.ctx.strict {
                    return Err(SyntaxError::new("Unexpected 'yield' identifier", tok.span));
                }
                Ok(Expr::new(ExprKind::Ident("yield".into()), tok.span))
            }
            TokenKind::Keyword(Keyword::Function) => {
                let func = self.parse_function(false, false)?;
                let span = func.span;
                Ok(Expr::new(ExprKind::Function(Box::new(func)), span))
            }
            TokenKind::Keyword(Keyword::Class) => {
                let cls = self.parse_class(false)?;
                let span = cls.span;
                Ok(Expr::new(ExprKind::Class(Box::new(cls)), span))
            }
            TokenKind::LParen => {
                let start = self.advance()?.span.start;
                let saved_in = self.ctx.allow_in;
                self.ctx.allow_in = true;
                let inner = self.parse_expr()?;
                self.ctx.allow_in = saved_in;
                let end = self.expect(TokenKind::RParen)?.span.end;
                Ok(Expr::new(
                    ExprKind::Paren(Box::new(inner)),
                    Span::new(start, end),
                ))
            }
            TokenKind::LBracket => self.parse_array_literal(),
            TokenKind::LBrace => self.parse_object_literal(),
            _ => Err(SyntaxError::new(
                format!("Unexpected token {:?}", self.cur.kind),
                self.cur.span,
            )),
        }
    }

    fn parse_array_literal(&mut self) -> Result<Expr, SyntaxError> {
        let start = self.expect(TokenKind::LBracket)?.span.start;
        let mut elements = Vec::new();
        while self.cur.kind != TokenKind::RBracket && self.cur.kind != TokenKind::Eof {
            if self.eat(TokenKind::Comma)? {
                elements.push(None);
                continue;
            }
            if self.eat(TokenKind::Ellipsis)? {
                elements.push(Some(ArrayElement::Spread(self.parse_assign_expr()?)));
            } else {
                elements.push(Some(ArrayElement::Expr(self.parse_assign_expr()?)));
            }
            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        let end = self.expect(TokenKind::RBracket)?.span.end;
        Ok(Expr::new(ExprKind::Array(elements), Span::new(start, end)))
    }

    fn parse_object_literal(&mut self) -> Result<Expr, SyntaxError> {
        let start = self.expect(TokenKind::LBrace)?.span.start;
        let mut props = Vec::new();
        while self.cur.kind != TokenKind::RBrace && self.cur.kind != TokenKind::Eof {
            let prop_start = self.cur.span.start;
            if self.eat(TokenKind::Ellipsis)? {
                let arg = self.parse_assign_expr()?;
                let end = arg.span.end;
                props.push(ObjectProp::Spread {
                    arg,
                    span: Span::new(prop_start, end),
                });
                if !self.eat(TokenKind::Comma)? {
                    break;
                }
                continue;
            }

            let mut is_async = false;
            let mut is_generator = false;
            let mut method_kind = MethodKind::Method;

            if self.is_ident_word("async")
                && !self.peek_token()?.line_terminator_before
                && !matches!(
                    self.peek_token()?.kind,
                    TokenKind::Colon
                        | TokenKind::Comma
                        | TokenKind::RBrace
                        | TokenKind::LParen
                        | TokenKind::Eq
                )
            {
                self.advance()?;
                is_async = true;
            }
            if self.eat(TokenKind::Star)? {
                is_generator = true;
            } else if !is_async
                && (self.is_ident_word("get") || self.is_ident_word("set"))
                && !matches!(
                    self.peek_token()?.kind,
                    TokenKind::Colon
                        | TokenKind::Comma
                        | TokenKind::RBrace
                        | TokenKind::LParen
                        | TokenKind::Eq
                )
            {
                method_kind = if self.is_ident_word("get") {
                    MethodKind::Get
                } else {
                    MethodKind::Set
                };
                self.advance()?;
            }

            let key = self.parse_prop_key(false)?;
            if self.cur.kind == TokenKind::LParen {
                let func = self.parse_function_params_and_body(
                    prop_start,
                    None,
                    is_async,
                    is_generator,
                    true,
                    false,
                )?;
                let end = func.span.end;
                props.push(ObjectProp::Method {
                    key,
                    kind: method_kind,
                    function: func,
                    span: Span::new(prop_start, end),
                });
            } else if self.eat(TokenKind::Colon)? {
                let value = self.parse_assign_expr()?;
                let end = value.span.end;
                props.push(ObjectProp::KeyValue {
                    key,
                    value,
                    shorthand: false,
                    span: Span::new(prop_start, end),
                });
            } else {
                // Shorthand property (`{ a }` or cover destructuring default `{ a = 1 }`).
                let PropKey::Ident(ref name) = key else {
                    return Err(SyntaxError::new(
                        "Expected ':' after property name",
                        self.cur.span,
                    ));
                };
                let ident_span = Span::new(prop_start, self.cur.span.start);
                let mut value = Expr::new(ExprKind::Ident(name.clone()), ident_span);
                if self.eat(TokenKind::Eq)? {
                    let default_val = self.parse_assign_expr()?;
                    let span = ident_span.merge(default_val.span);
                    value = Expr::new(
                        ExprKind::Assign {
                            op: AssignOp::Assign,
                            left: AssignTarget::Ident {
                                name: name.clone(),
                                span: ident_span,
                            },
                            right: Box::new(default_val),
                        },
                        span,
                    );
                }
                let end = value.span.end;
                props.push(ObjectProp::KeyValue {
                    key,
                    value,
                    shorthand: true,
                    span: Span::new(prop_start, end),
                });
            }

            if !self.eat(TokenKind::Comma)? {
                break;
            }
        }
        let end = self.expect(TokenKind::RBrace)?.span.end;
        Ok(Expr::new(ExprKind::Object(props), Span::new(start, end)))
    }

    fn parse_template_parts(
        &mut self,
        is_tagged: bool,
    ) -> Result<(Vec<TemplateElement>, Vec<Expr>, Span), SyntaxError> {
        let start = self.cur.span.start;
        match self.cur.kind.clone() {
            TokenKind::TemplateNoSub { raw, cooked } => {
                let tok = self.advance()?;
                if !is_tagged && cooked.is_none() {
                    return Err(SyntaxError::new(
                        "Invalid escape sequence in untagged template literal",
                        tok.span,
                    ));
                }
                Ok((
                    vec![TemplateElement {
                        raw,
                        cooked,
                        tail: true,
                        span: tok.span,
                    }],
                    Vec::new(),
                    tok.span,
                ))
            }
            TokenKind::TemplateHead { raw, cooked } => {
                let tok = self.advance()?;
                if !is_tagged && cooked.is_none() {
                    return Err(SyntaxError::new(
                        "Invalid escape sequence in untagged template literal",
                        tok.span,
                    ));
                }
                let mut quasis = vec![TemplateElement {
                    raw,
                    cooked,
                    tail: false,
                    span: tok.span,
                }];
                let mut exprs = Vec::new();
                let end;
                loop {
                    let saved_in = self.ctx.allow_in;
                    self.ctx.allow_in = true;
                    exprs.push(self.parse_expr()?);
                    self.ctx.allow_in = saved_in;

                    if self.cur.kind != TokenKind::RBrace {
                        return Err(SyntaxError::new(
                            "Expected '}' to close template expression",
                            self.cur.span,
                        ));
                    }
                    let rbrace_end = self.cur.span.end as usize;
                    self.peek = None;
                    let cont = self.lexer.lex_template_continuation(rbrace_end)?;
                    self.cur = self.lexer.next_token()?;
                    match cont.kind {
                        TokenKind::TemplateMiddle { raw, cooked } => {
                            if !is_tagged && cooked.is_none() {
                                return Err(SyntaxError::new(
                                    "Invalid escape sequence in untagged template literal",
                                    cont.span,
                                ));
                            }
                            quasis.push(TemplateElement {
                                raw,
                                cooked,
                                tail: false,
                                span: cont.span,
                            });
                        }
                        TokenKind::TemplateTail { raw, cooked } => {
                            if !is_tagged && cooked.is_none() {
                                return Err(SyntaxError::new(
                                    "Invalid escape sequence in untagged template literal",
                                    cont.span,
                                ));
                            }
                            end = cont.span.end;
                            quasis.push(TemplateElement {
                                raw,
                                cooked,
                                tail: true,
                                span: cont.span,
                            });
                            break;
                        }
                        _ => {
                            return Err(SyntaxError::new(
                                "Unexpected token in template literal",
                                cont.span,
                            ));
                        }
                    }
                }
                Ok((quasis, exprs, Span::new(start, end)))
            }
            _ => Err(SyntaxError::new("Expected template literal", self.cur.span)),
        }
    }

    // -----------------------------------------------------------------------
    // Cover Grammar: Expression -> Assignment Target / Destructuring Pattern
    // -----------------------------------------------------------------------

    fn expr_to_assign_target(&self, expr: Expr) -> Result<AssignTarget, SyntaxError> {
        match expr.kind {
            ExprKind::Ident(name) => {
                self.validate_assign_ident(&name, expr.span)?;
                Ok(AssignTarget::Ident {
                    name,
                    span: expr.span,
                })
            }
            ExprKind::Member {
                object,
                property,
                optional: false,
            } => Ok(AssignTarget::Member {
                object,
                property,
                span: expr.span,
            }),
            ExprKind::Paren(inner) => match inner.kind {
                ExprKind::Array(_) | ExprKind::Object(_) => Err(SyntaxError::new(
                    "Invalid parenthesized destructuring assignment target",
                    expr.span,
                )),
                _ => self.expr_to_assign_target(*inner),
            },
            ExprKind::Array(_) | ExprKind::Object(_) => {
                let pat = self.expr_to_pattern(expr)?;
                Ok(AssignTarget::Pattern(pat))
            }
            _ => Err(SyntaxError::new(
                "Invalid left-hand side in assignment",
                expr.span,
            )),
        }
    }

    fn expr_to_pattern(&self, expr: Expr) -> Result<Pattern, SyntaxError> {
        let span = expr.span;
        match expr.kind {
            ExprKind::Ident(name) => {
                self.validate_assign_ident(&name, span)?;
                Ok(Pattern::Ident { name, span })
            }
            ExprKind::Assign {
                op: AssignOp::Assign,
                left,
                right,
            } => {
                let pat = match left {
                    AssignTarget::Ident { name, span: s } => Pattern::Ident { name, span: s },
                    AssignTarget::Pattern(p) => p,
                    AssignTarget::Member { span: s, .. } => {
                        return Err(SyntaxError::new("Invalid assignment pattern element", s));
                    }
                };
                Ok(Pattern::Assign {
                    left: Box::new(pat),
                    right,
                    span,
                })
            }
            ExprKind::Array(items) => {
                let mut elements = Vec::with_capacity(items.len());
                let mut rest = None;
                let total = items.len();
                for (idx, item) in items.into_iter().enumerate() {
                    match item {
                        None => elements.push(None),
                        Some(ArrayElement::Expr(e)) => {
                            elements.push(Some(self.expr_to_pattern(e)?));
                        }
                        Some(ArrayElement::Spread(e)) => {
                            if idx + 1 != total {
                                return Err(SyntaxError::new(
                                    "Rest element must be last in destructuring assignment",
                                    e.span,
                                ));
                            }
                            rest = Some(Box::new(self.expr_to_pattern(e)?));
                        }
                    }
                }
                Ok(Pattern::Array {
                    elements,
                    rest,
                    span,
                })
            }
            ExprKind::Object(obj_props) => {
                let mut props = Vec::with_capacity(obj_props.len());
                let mut rest = None;
                let total = obj_props.len();
                for (idx, prop) in obj_props.into_iter().enumerate() {
                    match prop {
                        ObjectProp::KeyValue {
                            key,
                            value,
                            shorthand,
                            span: p_span,
                        } => {
                            props.push(ObjectPatProp {
                                key,
                                value: self.expr_to_pattern(value)?,
                                shorthand,
                                span: p_span,
                            });
                        }
                        ObjectProp::Spread { arg, .. } => {
                            if idx + 1 != total {
                                return Err(SyntaxError::new(
                                    "Rest property must be last in object destructuring",
                                    arg.span,
                                ));
                            }
                            rest = Some(Box::new(self.expr_to_pattern(arg)?));
                        }
                        ObjectProp::Method { span: m_span, .. } => {
                            return Err(SyntaxError::new(
                                "Method not allowed in destructuring pattern",
                                m_span,
                            ));
                        }
                    }
                }
                Ok(Pattern::Object { props, rest, span })
            }
            _ => Err(SyntaxError::new(
                "Invalid destructuring assignment pattern",
                span,
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Identifiers & Property Keys
    // -----------------------------------------------------------------------

    fn parse_prop_key(&mut self, allow_private: bool) -> Result<PropKey, SyntaxError> {
        match self.cur.kind.clone() {
            TokenKind::LBracket => {
                self.advance()?;
                let expr = self.parse_assign_expr()?;
                self.expect(TokenKind::RBracket)?;
                Ok(PropKey::Computed(Box::new(expr)))
            }
            TokenKind::PrivateIdent(name) if allow_private => {
                self.advance()?;
                Ok(PropKey::Private(name))
            }
            TokenKind::String { value, .. } => {
                self.advance()?;
                Ok(PropKey::String(value))
            }
            TokenKind::Number { value, .. } => {
                self.advance()?;
                Ok(PropKey::Number(value))
            }
            TokenKind::Ident { name, .. } => {
                self.advance()?;
                Ok(PropKey::Ident(name))
            }
            TokenKind::Keyword(kw) => {
                self.advance()?;
                Ok(PropKey::Ident(kw.as_str().to_owned()))
            }
            _ => Err(SyntaxError::new(
                format!("Expected property name, found {:?}", self.cur.kind),
                self.cur.span,
            )),
        }
    }

    fn parse_prop_ident(&mut self) -> Result<(String, Span), SyntaxError> {
        match self.cur.kind.clone() {
            TokenKind::Ident { name, .. } => {
                let tok = self.advance()?;
                Ok((name, tok.span))
            }
            TokenKind::Keyword(kw) => {
                let tok = self.advance()?;
                Ok((kw.as_str().to_owned(), tok.span))
            }
            _ => Err(SyntaxError::new(
                format!("Expected property identifier, found {:?}", self.cur.kind),
                self.cur.span,
            )),
        }
    }

    fn parse_ident_name(&mut self) -> Result<(String, Span), SyntaxError> {
        match self.cur.kind.clone() {
            TokenKind::Ident { name, .. } => {
                let tok = self.advance()?;
                Ok((name, tok.span))
            }
            TokenKind::Keyword(Keyword::Await)
                if !self.ctx.in_async && self.source_type != SourceType::Module =>
            {
                let tok = self.advance()?;
                Ok(("await".into(), tok.span))
            }
            TokenKind::Keyword(Keyword::Yield) if !self.ctx.in_generator && !self.ctx.strict => {
                let tok = self.advance()?;
                Ok(("yield".into(), tok.span))
            }
            _ => Err(SyntaxError::new(
                format!("Expected identifier, found {:?}", self.cur.kind),
                self.cur.span,
            )),
        }
    }

    fn parse_binding_ident_name(&mut self) -> Result<(String, Span), SyntaxError> {
        self.parse_ident_name()
    }

    fn expect_contextual(&mut self, word: &str) -> Result<Token, SyntaxError> {
        if self.is_ident_word(word) {
            self.advance()
        } else {
            Err(SyntaxError::new(
                format!("Expected '{word}', found {:?}", self.cur.kind),
                self.cur.span,
            ))
        }
    }

    fn expect_string(&mut self) -> Result<(String, u32), SyntaxError> {
        if let TokenKind::String { value, .. } = self.cur.kind.clone() {
            let tok = self.advance()?;
            Ok((value, tok.span.end))
        } else {
            Err(SyntaxError::new(
                format!("Expected string literal, found {:?}", self.cur.kind),
                self.cur.span,
            ))
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum BinOrLogOp {
    Binary(BinaryOp),
    Logical(LogicalOp),
}

fn token_to_assign_op(kind: &TokenKind) -> Option<AssignOp> {
    match kind {
        TokenKind::Eq => Some(AssignOp::Assign),
        TokenKind::PlusEq => Some(AssignOp::AddAssign),
        TokenKind::MinusEq => Some(AssignOp::SubAssign),
        TokenKind::StarEq => Some(AssignOp::MulAssign),
        TokenKind::SlashEq => Some(AssignOp::DivAssign),
        TokenKind::PercentEq => Some(AssignOp::RemAssign),
        TokenKind::StarStarEq => Some(AssignOp::ExpAssign),
        TokenKind::LShiftEq => Some(AssignOp::ShlAssign),
        TokenKind::RShiftEq => Some(AssignOp::ShrAssign),
        TokenKind::URShiftEq => Some(AssignOp::UShrAssign),
        TokenKind::AmpEq => Some(AssignOp::BitAndAssign),
        TokenKind::PipeEq => Some(AssignOp::BitOrAssign),
        TokenKind::CaretEq => Some(AssignOp::BitXorAssign),
        TokenKind::AmpAmpEq => Some(AssignOp::AndAssign),
        TokenKind::PipePipeEq => Some(AssignOp::OrAssign),
        TokenKind::QuestionQuestionEq => Some(AssignOp::NullishAssign),
        _ => None,
    }
}

fn is_bare_nullish(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Logical {
            op: LogicalOp::Nullish,
            ..
        }
    )
}

fn is_bare_and_or(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Logical {
            op: LogicalOp::And | LogicalOp::Or,
            ..
        }
    )
}
