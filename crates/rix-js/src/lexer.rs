//! Spec-compliant ECMAScript 2024 Lexer.
//!
//! Supports Unicode identifiers and escapes, private identifiers, numeric separators,
//! BigInt literals, template literals (including nested expressions), regex literals
//! (parser-disambiguated), hashbangs, and line-terminator tracking for ASI.

use crate::ast::{Span, SyntaxError};
use unicode_ident::{is_xid_continue, is_xid_start};

/// ECMAScript 2024 reserved keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Await,
    Break,
    Case,
    Catch,
    Class,
    Const,
    Continue,
    Debugger,
    Default,
    Delete,
    Do,
    Else,
    Enum,
    Export,
    Extends,
    False,
    Finally,
    For,
    Function,
    If,
    Import,
    In,
    InstanceOf,
    New,
    Null,
    Return,
    Super,
    Switch,
    This,
    Throw,
    True,
    Try,
    TypeOf,
    Var,
    Void,
    While,
    With,
    Yield,
}

impl Keyword {
    pub fn from_keyword_str(s: &str) -> Option<Self> {
        match s {
            "await" => Some(Self::Await),
            "break" => Some(Self::Break),
            "case" => Some(Self::Case),
            "catch" => Some(Self::Catch),
            "class" => Some(Self::Class),
            "const" => Some(Self::Const),
            "continue" => Some(Self::Continue),
            "debugger" => Some(Self::Debugger),
            "default" => Some(Self::Default),
            "delete" => Some(Self::Delete),
            "do" => Some(Self::Do),
            "else" => Some(Self::Else),
            "enum" => Some(Self::Enum),
            "export" => Some(Self::Export),
            "extends" => Some(Self::Extends),
            "false" => Some(Self::False),
            "finally" => Some(Self::Finally),
            "for" => Some(Self::For),
            "function" => Some(Self::Function),
            "if" => Some(Self::If),
            "import" => Some(Self::Import),
            "in" => Some(Self::In),
            "instanceof" => Some(Self::InstanceOf),
            "new" => Some(Self::New),
            "null" => Some(Self::Null),
            "return" => Some(Self::Return),
            "super" => Some(Self::Super),
            "switch" => Some(Self::Switch),
            "this" => Some(Self::This),
            "throw" => Some(Self::Throw),
            "true" => Some(Self::True),
            "try" => Some(Self::Try),
            "typeof" => Some(Self::TypeOf),
            "var" => Some(Self::Var),
            "void" => Some(Self::Void),
            "while" => Some(Self::While),
            "with" => Some(Self::With),
            "yield" => Some(Self::Yield),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Await => "await",
            Self::Break => "break",
            Self::Case => "case",
            Self::Catch => "catch",
            Self::Class => "class",
            Self::Const => "const",
            Self::Continue => "continue",
            Self::Debugger => "debugger",
            Self::Default => "default",
            Self::Delete => "delete",
            Self::Do => "do",
            Self::Else => "else",
            Self::Enum => "enum",
            Self::Export => "export",
            Self::Extends => "extends",
            Self::False => "false",
            Self::Finally => "finally",
            Self::For => "for",
            Self::Function => "function",
            Self::If => "if",
            Self::Import => "import",
            Self::In => "in",
            Self::InstanceOf => "instanceof",
            Self::New => "new",
            Self::Null => "null",
            Self::Return => "return",
            Self::Super => "super",
            Self::Switch => "switch",
            Self::This => "this",
            Self::Throw => "throw",
            Self::True => "true",
            Self::Try => "try",
            Self::TypeOf => "typeof",
            Self::Var => "var",
            Self::Void => "void",
            Self::While => "while",
            Self::With => "with",
            Self::Yield => "yield",
        }
    }
}

/// Strict-mode future reserved identifiers.
pub fn is_strict_reserved_word(s: &str) -> bool {
    matches!(
        s,
        "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Eof,
    /// Identifier (including contextual keywords). `escaped` is true if any
    /// Unicode escape (`\uXXXX`) appeared in the identifier.
    Ident {
        name: String,
        escaped: bool,
    },
    PrivateIdent(String),
    Keyword(Keyword),
    Number {
        value: f64,
        legacy_octal: bool,
    },
    BigInt(String),
    String {
        value: String,
        has_legacy_octal_escape: bool,
        has_escape: bool,
    },
    TemplateNoSub {
        raw: String,
        cooked: Option<String>,
    },
    TemplateHead {
        raw: String,
        cooked: Option<String>,
    },
    TemplateMiddle {
        raw: String,
        cooked: Option<String>,
    },
    TemplateTail {
        raw: String,
        cooked: Option<String>,
    },
    // Punctuators
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Dot,
    Ellipsis,
    Semicolon,
    Comma,
    Colon,
    Question,
    QuestionDot,
    Arrow,
    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    StarStar,
    PlusPlus,
    MinusMinus,
    Bang,
    Tilde,
    Lt,
    LtEq,
    Gt,
    GtEq,
    EqEq,
    BangEq,
    EqEqEq,
    BangEqEq,
    LShift,
    RShift,
    URShift,
    Amp,
    Pipe,
    Caret,
    AmpAmp,
    PipePipe,
    QuestionQuestion,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    StarStarEq,
    LShiftEq,
    RShiftEq,
    URShiftEq,
    AmpEq,
    PipeEq,
    CaretEq,
    AmpAmpEq,
    PipePipeEq,
    QuestionQuestionEq,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// True if at least one LineTerminator occurred in trivia immediately
    /// preceding this token (required for Automatic Semicolon Insertion).
    pub line_terminator_before: bool,
}

/// Lightweight lexer cursor checkpoint for deterministic lookahead.
#[derive(Debug, Clone, Copy)]
pub struct LexerCheckpoint {
    pos: usize,
}

/// Stateful UTF-8 ECMAScript 2024 lexer.
pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        let mut lexer = Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        };
        // Skip optional UTF-8 BOM.
        if lexer.src.starts_with('\u{FEFF}') {
            lexer.pos += 3;
        }
        // Skip ES2023 Hashbang comment (`#!...`) at byte 0.
        if lexer.bytes.get(lexer.pos..lexer.pos + 2) == Some(b"#!") {
            lexer.pos += 2;
            while let Some(ch) = lexer.peek_char() {
                if is_line_terminator(ch) {
                    break;
                }
                lexer.pos += ch.len_utf8();
            }
        }
        lexer
    }

    #[inline]
    pub fn checkpoint(&self) -> LexerCheckpoint {
        LexerCheckpoint { pos: self.pos }
    }

    #[inline]
    pub fn restore(&mut self, cp: LexerCheckpoint) {
        self.pos = cp.pos;
    }

    #[inline]
    pub fn pos(&self) -> usize {
        self.pos
    }

    #[inline]
    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    #[inline]
    fn peek_byte_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    #[inline]
    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    #[inline]
    fn next_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    /// Skips whitespace and comments; returns `true` if any LineTerminator was seen.
    fn skip_trivia(&mut self) -> Result<bool, SyntaxError> {
        let mut saw_newline = false;
        while let Some(ch) = self.peek_char() {
            if is_line_terminator(ch) {
                saw_newline = true;
                self.pos += ch.len_utf8();
                continue;
            }
            if is_whitespace(ch) {
                self.pos += ch.len_utf8();
                continue;
            }
            if ch == '/' {
                match self.peek_byte_at(1) {
                    Some(b'/') => {
                        self.pos += 2;
                        while let Some(c) = self.peek_char() {
                            if is_line_terminator(c) {
                                break;
                            }
                            self.pos += c.len_utf8();
                        }
                        continue;
                    }
                    Some(b'*') => {
                        let start = self.pos as u32;
                        self.pos += 2;
                        let mut closed = false;
                        while let Some(c) = self.next_char() {
                            if is_line_terminator(c) {
                                saw_newline = true;
                            } else if c == '*' && self.peek_byte() == Some(b'/') {
                                self.pos += 1;
                                closed = true;
                                break;
                            }
                        }
                        if !closed {
                            return Err(SyntaxError::new(
                                "Unterminated block comment",
                                Span::new(start, self.pos as u32),
                            ));
                        }
                        continue;
                    }
                    _ => break,
                }
            }
            break;
        }
        Ok(saw_newline)
    }

    /// Reads the next regular token.
    pub fn next_token(&mut self) -> Result<Token, SyntaxError> {
        let line_terminator_before = self.skip_trivia()?;
        let start = self.pos as u32;

        let Some(b) = self.peek_byte() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: Span::new(start, start),
                line_terminator_before,
            });
        };

        // Private identifier: `#ident`
        if b == b'#' {
            self.pos += 1;
            let (name, _) = self.read_identifier_parts(start)?;
            return Ok(Token {
                kind: TokenKind::PrivateIdent(name),
                span: Span::new(start, self.pos as u32),
                line_terminator_before,
            });
        }

        // Identifiers and keywords (including leading `\uXXXX`).
        if b == b'\\' || is_ascii_id_start(b) || (!b.is_ascii() && self.is_unicode_id_start()) {
            let (name, escaped) = self.read_identifier_parts(start)?;
            let kind = if !escaped {
                if let Some(kw) = Keyword::from_keyword_str(&name) {
                    TokenKind::Keyword(kw)
                } else {
                    TokenKind::Ident { name, escaped }
                }
            } else {
                TokenKind::Ident { name, escaped }
            };
            return Ok(Token {
                kind,
                span: Span::new(start, self.pos as u32),
                line_terminator_before,
            });
        }

        // Numeric literal (`0..9` or `.0..9`).
        if b.is_ascii_digit()
            || (b == b'.' && self.peek_byte_at(1).is_some_and(|n| n.is_ascii_digit()))
        {
            let kind = self.read_number(start)?;
            return Ok(Token {
                kind,
                span: Span::new(start, self.pos as u32),
                line_terminator_before,
            });
        }

        // String literal (`'...'` or `"..."`).
        if b == b'\'' || b == b'"' {
            let kind = self.read_string(b, start)?;
            return Ok(Token {
                kind,
                span: Span::new(start, self.pos as u32),
                line_terminator_before,
            });
        }

        // Template literal start (`` ` ``).
        if b == b'`' {
            self.pos += 1;
            let kind = self.read_template_chunk(start, true)?;
            return Ok(Token {
                kind,
                span: Span::new(start, self.pos as u32),
                line_terminator_before,
            });
        }

        // Punctuators and operators.
        let kind = self.read_punctuator(start)?;
        Ok(Token {
            kind,
            span: Span::new(start, self.pos as u32),
            line_terminator_before,
        })
    }

    /// Resumes lexing a template literal immediately after a closing `}` of `${expr}`.
    /// `rbrace_end` is the byte offset right after the `}` token.
    pub fn lex_template_continuation(&mut self, rbrace_end: usize) -> Result<Token, SyntaxError> {
        self.pos = rbrace_end;
        let start = self.pos as u32;
        let kind = self.read_template_chunk(start, false)?;
        Ok(Token {
            kind,
            span: Span::new(start, self.pos as u32),
            line_terminator_before: false,
        })
    }

    /// Re-lexes a `/` or `/=` token starting at `slash_start` as a Regular Expression literal.
    pub fn lex_regex_literal(
        &mut self,
        slash_start: usize,
    ) -> Result<(String, String, Span), SyntaxError> {
        self.pos = slash_start;
        if self.peek_byte() != Some(b'/') {
            return Err(SyntaxError::new(
                "Expected '/' to start regular expression literal",
                Span::new(slash_start as u32, slash_start as u32),
            ));
        }
        self.pos += 1; // consume opening '/'
        let mut pattern = String::new();
        let mut in_class = false;
        let mut escaped = false;
        let mut closed = false;

        while let Some(ch) = self.next_char() {
            if is_line_terminator(ch) {
                return Err(SyntaxError::new(
                    "Unterminated regular expression literal",
                    Span::new(slash_start as u32, self.pos as u32),
                ));
            }
            if escaped {
                pattern.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => {
                    pattern.push(ch);
                    escaped = true;
                }
                '[' => {
                    in_class = true;
                    pattern.push(ch);
                }
                ']' => {
                    in_class = false;
                    pattern.push(ch);
                }
                '/' if !in_class => {
                    closed = true;
                    break;
                }
                _ => pattern.push(ch),
            }
        }
        if !closed {
            return Err(SyntaxError::new(
                "Unterminated regular expression literal",
                Span::new(slash_start as u32, self.pos as u32),
            ));
        }

        // Read flags (ID_Continue characters immediately after closing `/`).
        let mut flags = String::new();
        let mut seen_mask: u8 = 0;
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_alphabetic() || ch == '$' || ch == '_' || is_xid_continue(ch) {
                self.pos += ch.len_utf8();
                let bit = match ch {
                    'd' => 1 << 0,
                    'g' => 1 << 1,
                    'i' => 1 << 2,
                    'm' => 1 << 3,
                    's' => 1 << 4,
                    'u' => 1 << 5,
                    'v' => 1 << 6,
                    'y' => 1 << 7,
                    _ => {
                        return Err(SyntaxError::new(
                            format!("Invalid regular expression flag '{ch}'"),
                            Span::new(slash_start as u32, self.pos as u32),
                        ));
                    }
                };
                if (seen_mask & bit) != 0 {
                    return Err(SyntaxError::new(
                        format!("Duplicate regular expression flag '{ch}'"),
                        Span::new(slash_start as u32, self.pos as u32),
                    ));
                }
                seen_mask |= bit;
                flags.push(ch);
            } else {
                break;
            }
        }
        // ES2024: `u` and `v` flags cannot be combined.
        if (seen_mask & (1 << 5)) != 0 && (seen_mask & (1 << 6)) != 0 {
            return Err(SyntaxError::new(
                "Regular expression cannot have both 'u' and 'v' flags",
                Span::new(slash_start as u32, self.pos as u32),
            ));
        }

        Ok((
            pattern,
            flags,
            Span::new(slash_start as u32, self.pos as u32),
        ))
    }

    fn is_unicode_id_start(&self) -> bool {
        self.peek_char()
            .is_some_and(|c| c == '$' || c == '_' || is_xid_start(c))
    }

    fn read_identifier_parts(&mut self, start: u32) -> Result<(String, bool), SyntaxError> {
        let mut name = String::new();
        let mut escaped = false;

        let first = if self.peek_byte() == Some(b'\\') {
            escaped = true;
            self.read_unicode_escape(start)?
        } else {
            self.next_char().ok_or_else(|| {
                SyntaxError::new("Expected identifier", Span::new(start, self.pos as u32))
            })?
        };
        if !(first == '$' || first == '_' || is_xid_start(first)) {
            return Err(SyntaxError::new(
                format!("Invalid identifier start character '{first}'"),
                Span::new(start, self.pos as u32),
            ));
        }
        name.push(first);

        loop {
            match self.peek_byte() {
                Some(b'\\') => {
                    escaped = true;
                    let ch = self.read_unicode_escape(start)?;
                    if !is_id_continue(ch) {
                        return Err(SyntaxError::new(
                            format!("Invalid escaped identifier character '{ch}'"),
                            Span::new(start, self.pos as u32),
                        ));
                    }
                    name.push(ch);
                }
                Some(b) if is_ascii_id_continue(b) => {
                    name.push(b as char);
                    self.pos += 1;
                }
                Some(b) if !b.is_ascii() => {
                    if let Some(ch) = self.peek_char() {
                        if is_id_continue(ch) {
                            name.push(ch);
                            self.pos += ch.len_utf8();
                            continue;
                        }
                    }
                    break;
                }
                _ => break,
            }
        }

        Ok((name, escaped))
    }

    fn read_unicode_escape(&mut self, start: u32) -> Result<char, SyntaxError> {
        // Expect `\uXXXX` or `\u{XXXXX}`.
        if self.peek_byte() != Some(b'\\') || self.peek_byte_at(1) != Some(b'u') {
            return Err(SyntaxError::new(
                "Invalid Unicode escape in identifier",
                Span::new(start, self.pos as u32),
            ));
        }
        self.pos += 2;
        let cp = self.read_unicode_escape_codepoint(start)?;
        char::from_u32(cp).ok_or_else(|| {
            SyntaxError::new(
                "Invalid Unicode scalar value in identifier",
                Span::new(start, self.pos as u32),
            )
        })
    }

    fn read_unicode_escape_codepoint(&mut self, start: u32) -> Result<u32, SyntaxError> {
        if self.peek_byte() == Some(b'{') {
            self.pos += 1;
            let mut val: u32 = 0;
            let mut digits = 0usize;
            while let Some(b) = self.peek_byte() {
                if b == b'}' {
                    self.pos += 1;
                    if digits == 0 || val > 0x10_FFFF {
                        return Err(SyntaxError::new(
                            "Invalid Unicode code point escape",
                            Span::new(start, self.pos as u32),
                        ));
                    }
                    return Ok(val);
                }
                let d = hex_val(b).ok_or_else(|| {
                    SyntaxError::new(
                        "Invalid hex digit in Unicode escape",
                        Span::new(start, self.pos as u32),
                    )
                })?;
                val = val.saturating_mul(16).saturating_add(d);
                digits += 1;
                self.pos += 1;
            }
            return Err(SyntaxError::new(
                "Unterminated Unicode code point escape",
                Span::new(start, self.pos as u32),
            ));
        }

        let mut val: u32 = 0;
        for _ in 0..4 {
            let b = self.peek_byte().ok_or_else(|| {
                SyntaxError::new(
                    "Incomplete 4-digit Unicode escape",
                    Span::new(start, self.pos as u32),
                )
            })?;
            let d = hex_val(b).ok_or_else(|| {
                SyntaxError::new(
                    "Invalid hex digit in Unicode escape",
                    Span::new(start, self.pos as u32),
                )
            })?;
            val = (val << 4) | d;
            self.pos += 1;
        }
        Ok(val)
    }

    fn read_number(&mut self, start: u32) -> Result<TokenKind, SyntaxError> {
        let mut legacy_octal = false;
        if self.peek_byte() == Some(b'0') {
            match self.peek_byte_at(1) {
                Some(b'x' | b'X') => {
                    self.pos += 2;
                    let digits = self.read_radix_digits(16, start)?;
                    return self.finish_radix_number(16, &digits, start);
                }
                Some(b'b' | b'B') => {
                    self.pos += 2;
                    let digits = self.read_radix_digits(2, start)?;
                    return self.finish_radix_number(2, &digits, start);
                }
                Some(b'o' | b'O') => {
                    self.pos += 2;
                    let digits = self.read_radix_digits(8, start)?;
                    return self.finish_radix_number(8, &digits, start);
                }
                Some(b'0'..=b'9') => {
                    // Legacy octal or non-octal decimal integer with leading 0.
                    legacy_octal = true;
                }
                _ => {}
            }
        }

        let mut buf = String::new();
        let mut is_float = false;

        if self.peek_byte() != Some(b'.') {
            let int_part = self.read_decimal_digits(start, !legacy_octal)?;
            buf.push_str(&int_part);
        }

        if self.peek_byte() == Some(b'.') {
            is_float = true;
            buf.push('.');
            self.pos += 1;
            if self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
                let frac = self.read_decimal_digits(start, true)?;
                buf.push_str(&frac);
            } else if self.peek_byte() == Some(b'_') {
                return Err(SyntaxError::new(
                    "Numeric separator cannot appear immediately after decimal point",
                    Span::new(start, self.pos as u32),
                ));
            }
        }

        if matches!(self.peek_byte(), Some(b'e' | b'E')) {
            is_float = true;
            buf.push('e');
            self.pos += 1;
            if let Some(sign @ (b'+' | b'-')) = self.peek_byte() {
                buf.push(sign as char);
                self.pos += 1;
            }
            let exp = self.read_decimal_digits(start, true)?;
            buf.push_str(&exp);
        }

        // BigInt suffix `n`.
        if self.peek_byte() == Some(b'n') {
            if is_float || legacy_octal {
                return Err(SyntaxError::new(
                    "Invalid BigInt literal",
                    Span::new(start, self.pos as u32 + 1),
                ));
            }
            self.pos += 1;
            self.check_no_ident_after_number(start)?;
            return Ok(TokenKind::BigInt(buf));
        }

        self.check_no_ident_after_number(start)?;

        // Handle legacy octal (e.g. `077`) if all digits are 0..=7 and not a float.
        if legacy_octal && !is_float && buf.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            let mut acc = 0f64;
            for b in buf.bytes() {
                acc = acc * 8.0 + f64::from(b - b'0');
            }
            return Ok(TokenKind::Number {
                value: acc,
                legacy_octal: true,
            });
        }

        let value = buf.parse::<f64>().map_err(|_| {
            SyntaxError::new("Invalid numeric literal", Span::new(start, self.pos as u32))
        })?;
        Ok(TokenKind::Number {
            value,
            legacy_octal,
        })
    }

    fn finish_radix_number(
        &mut self,
        radix: u32,
        digits: &str,
        start: u32,
    ) -> Result<TokenKind, SyntaxError> {
        if self.peek_byte() == Some(b'n') {
            self.pos += 1;
            self.check_no_ident_after_number(start)?;
            let prefix = match radix {
                2 => "0b",
                8 => "0o",
                16 => "0x",
                _ => "",
            };
            return Ok(TokenKind::BigInt(format!("{prefix}{digits}")));
        }
        self.check_no_ident_after_number(start)?;
        let mut value = 0f64;
        let base = f64::from(radix);
        for b in digits.bytes() {
            let d = hex_val(b).unwrap_or(0);
            value = value * base + f64::from(d);
        }
        Ok(TokenKind::Number {
            value,
            legacy_octal: false,
        })
    }

    fn read_radix_digits(&mut self, radix: u32, start: u32) -> Result<String, SyntaxError> {
        let mut out = String::new();
        let mut prev_sep = false;
        let mut first = true;
        while let Some(b) = self.peek_byte() {
            if b == b'_' {
                if first || prev_sep {
                    return Err(SyntaxError::new(
                        "Invalid numeric separator",
                        Span::new(start, self.pos as u32 + 1),
                    ));
                }
                prev_sep = true;
                self.pos += 1;
                continue;
            }
            if let Some(d) = hex_val(b) {
                if d < radix {
                    out.push(b as char);
                    self.pos += 1;
                    prev_sep = false;
                    first = false;
                    continue;
                }
            }
            break;
        }
        if out.is_empty() || prev_sep {
            return Err(SyntaxError::new(
                "Invalid radix numeric literal",
                Span::new(start, self.pos as u32),
            ));
        }
        Ok(out)
    }

    fn read_decimal_digits(
        &mut self,
        start: u32,
        allow_separators: bool,
    ) -> Result<String, SyntaxError> {
        let mut out = String::new();
        let mut prev_sep = false;
        let mut first = true;
        while let Some(b) = self.peek_byte() {
            if b == b'_' {
                if !allow_separators || first || prev_sep {
                    return Err(SyntaxError::new(
                        "Invalid numeric separator",
                        Span::new(start, self.pos as u32 + 1),
                    ));
                }
                prev_sep = true;
                self.pos += 1;
                continue;
            }
            if b.is_ascii_digit() {
                out.push(b as char);
                self.pos += 1;
                prev_sep = false;
                first = false;
            } else {
                break;
            }
        }
        if out.is_empty() || prev_sep {
            return Err(SyntaxError::new(
                "Invalid numeric literal",
                Span::new(start, self.pos as u32),
            ));
        }
        Ok(out)
    }

    fn check_no_ident_after_number(&self, start: u32) -> Result<(), SyntaxError> {
        if let Some(ch) = self.peek_char() {
            if ch == '$' || ch == '_' || is_xid_start(ch) || ch.is_ascii_digit() {
                return Err(SyntaxError::new(
                    "Identifier directly after numeric literal",
                    Span::new(start, self.pos as u32),
                ));
            }
        }
        Ok(())
    }

    fn read_string(&mut self, quote: u8, start: u32) -> Result<TokenKind, SyntaxError> {
        self.pos += 1; // consume opening quote
        let mut out = String::new();
        let mut has_legacy_octal_escape = false;
        let mut has_escape = false;

        while let Some(ch) = self.next_char() {
            if ch as u32 == u32::from(quote) {
                return Ok(TokenKind::String {
                    value: out,
                    has_legacy_octal_escape,
                    has_escape,
                });
            }
            if ch == '\n' || ch == '\r' {
                return Err(SyntaxError::new(
                    "Unterminated string literal",
                    Span::new(start, self.pos as u32),
                ));
            }
            if ch == '\\' {
                has_escape = true;
                let (decoded, is_octal) = self.read_escape_sequence(start, false)?;
                if is_octal {
                    has_legacy_octal_escape = true;
                }
                if let Some(s) = decoded {
                    out.push_str(&s);
                }
            } else {
                out.push(ch);
            }
        }

        Err(SyntaxError::new(
            "Unterminated string literal",
            Span::new(start, self.pos as u32),
        ))
    }

    fn read_template_chunk(&mut self, start: u32, is_head: bool) -> Result<TokenKind, SyntaxError> {
        let mut raw = String::new();
        let mut cooked = Some(String::new());

        while let Some(ch) = self.next_char() {
            match ch {
                '`' => {
                    return Ok(if is_head {
                        TokenKind::TemplateNoSub { raw, cooked }
                    } else {
                        TokenKind::TemplateTail { raw, cooked }
                    });
                }
                '$' if self.peek_byte() == Some(b'{') => {
                    self.pos += 1; // consume '{'
                    return Ok(if is_head {
                        TokenKind::TemplateHead { raw, cooked }
                    } else {
                        TokenKind::TemplateMiddle { raw, cooked }
                    });
                }
                '\r' => {
                    if self.peek_byte() == Some(b'\n') {
                        self.pos += 1;
                    }
                    raw.push('\n');
                    if let Some(c) = &mut cooked {
                        c.push('\n');
                    }
                }
                '\\' => {
                    let esc_start = self.pos;
                    match self.read_escape_sequence(start, true) {
                        Ok((decoded, is_octal)) => {
                            let raw_slice = &self.src[esc_start..self.pos];
                            raw.push('\\');
                            push_normalized_raw(&mut raw, raw_slice);
                            if is_octal {
                                cooked = None;
                            } else if let (Some(c), Some(s)) = (&mut cooked, decoded) {
                                c.push_str(&s);
                            }
                        }
                        Err(_) => {
                            // Tagged templates allow invalid escapes (`cooked = None`).
                            let raw_slice = &self.src[esc_start..self.pos];
                            raw.push('\\');
                            push_normalized_raw(&mut raw, raw_slice);
                            cooked = None;
                        }
                    }
                }
                _ => {
                    raw.push(ch);
                    if let Some(c) = &mut cooked {
                        c.push(ch);
                    }
                }
            }
        }

        Err(SyntaxError::new(
            "Unterminated template literal",
            Span::new(start, self.pos as u32),
        ))
    }

    /// Reads the character(s) after a backslash `\`. Returns `(Option<String>, is_legacy_octal)`.
    fn read_escape_sequence(
        &mut self,
        start: u32,
        _in_template: bool,
    ) -> Result<(Option<String>, bool), SyntaxError> {
        let Some(ch) = self.next_char() else {
            return Err(SyntaxError::new(
                "Unexpected end of input in escape sequence",
                Span::new(start, self.pos as u32),
            ));
        };
        match ch {
            '\n' | '\u{2028}' | '\u{2029}' => Ok((None, false)),
            '\r' => {
                if self.peek_byte() == Some(b'\n') {
                    self.pos += 1;
                }
                Ok((None, false))
            }
            'n' => Ok((Some("\n".into()), false)),
            'r' => Ok((Some("\r".into()), false)),
            't' => Ok((Some("\t".into()), false)),
            'b' => Ok((Some("\u{0008}".into()), false)),
            'f' => Ok((Some("\u{000C}".into()), false)),
            'v' => Ok((Some("\u{000B}".into()), false)),
            '\\' => Ok((Some("\\".into()), false)),
            '\'' => Ok((Some("'".into()), false)),
            '"' => Ok((Some("\"".into()), false)),
            '`' => Ok((Some("`".into()), false)),
            '0' => {
                if self.peek_byte().is_some_and(|b| b.is_ascii_digit()) {
                    // Legacy octal escape `\00`..`\07` (or `\08`/`\09`).
                    let mut val = 0u32;
                    for _ in 0..2 {
                        if let Some(b @ b'0'..=b'7') = self.peek_byte() {
                            val = val * 8 + u32::from(b - b'0');
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    let c = char::from_u32(val).unwrap_or('\0');
                    Ok((Some(c.to_string()), true))
                } else {
                    Ok((Some("\0".into()), false))
                }
            }
            '1'..='7' => {
                let mut val = u32::from(ch as u8 - b'0');
                let max_extra = if val <= 3 { 2 } else { 1 };
                for _ in 0..max_extra {
                    if let Some(b @ b'0'..=b'7') = self.peek_byte() {
                        val = val * 8 + u32::from(b - b'0');
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let c = char::from_u32(val).unwrap_or('\u{FFFD}');
                Ok((Some(c.to_string()), true))
            }
            '8' | '9' => Ok((Some(ch.to_string()), true)),
            'x' => {
                let mut val = 0u32;
                for _ in 0..2 {
                    let Some(b) = self.peek_byte() else {
                        return Err(SyntaxError::new(
                            "Incomplete hex escape",
                            Span::new(start, self.pos as u32),
                        ));
                    };
                    let Some(d) = hex_val(b) else {
                        return Err(SyntaxError::new(
                            "Invalid hex escape",
                            Span::new(start, self.pos as u32),
                        ));
                    };
                    val = (val << 4) | d;
                    self.pos += 1;
                }
                let c = char::from_u32(val).unwrap_or('\u{FFFD}');
                Ok((Some(c.to_string()), false))
            }
            'u' => {
                let cp = self.read_unicode_escape_codepoint(start)?;
                // Combine UTF-16 surrogate pair if followed immediately by `\uDC00`..`\uDFFF`.
                if (0xD800..=0xDBFF).contains(&cp)
                    && self.peek_byte() == Some(b'\\')
                    && self.peek_byte_at(1) == Some(b'u')
                    && self.peek_byte_at(2) != Some(b'{')
                {
                    let saved = self.pos;
                    self.pos += 2;
                    if let Ok(low) = self.read_unicode_escape_codepoint(start) {
                        if (0xDC00..=0xDFFF).contains(&low) {
                            let combined = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                            if let Some(c) = char::from_u32(combined) {
                                return Ok((Some(c.to_string()), false));
                            }
                        }
                    }
                    self.pos = saved;
                }
                let decoded = char::from_u32(cp)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "\u{FFFD}".into());
                Ok((Some(decoded), false))
            }
            _ => Ok((Some(ch.to_string()), false)),
        }
    }

    fn read_punctuator(&mut self, start: u32) -> Result<TokenKind, SyntaxError> {
        let b0 = self.bytes[self.pos];
        let b1 = self.peek_byte_at(1);
        let b2 = self.peek_byte_at(2);
        let b3 = self.peek_byte_at(3);

        let (kind, len) = match (b0, b1, b2, b3) {
            (b'>', Some(b'>'), Some(b'>'), Some(b'=')) => (TokenKind::URShiftEq, 4),
            (b'.', Some(b'.'), Some(b'.'), _) => (TokenKind::Ellipsis, 3),
            (b'=', Some(b'='), Some(b'='), _) => (TokenKind::EqEqEq, 3),
            (b'!', Some(b'='), Some(b'='), _) => (TokenKind::BangEqEq, 3),
            (b'>', Some(b'>'), Some(b'>'), _) => (TokenKind::URShift, 3),
            (b'<', Some(b'<'), Some(b'='), _) => (TokenKind::LShiftEq, 3),
            (b'>', Some(b'>'), Some(b'='), _) => (TokenKind::RShiftEq, 3),
            (b'*', Some(b'*'), Some(b'='), _) => (TokenKind::StarStarEq, 3),
            (b'&', Some(b'&'), Some(b'='), _) => (TokenKind::AmpAmpEq, 3),
            (b'|', Some(b'|'), Some(b'='), _) => (TokenKind::PipePipeEq, 3),
            (b'?', Some(b'?'), Some(b'='), _) => (TokenKind::QuestionQuestionEq, 3),
            (b'=', Some(b'>'), _, _) => (TokenKind::Arrow, 2),
            (b'=', Some(b'='), _, _) => (TokenKind::EqEq, 2),
            (b'!', Some(b'='), _, _) => (TokenKind::BangEq, 2),
            (b'<', Some(b'='), _, _) => (TokenKind::LtEq, 2),
            (b'>', Some(b'='), _, _) => (TokenKind::GtEq, 2),
            (b'<', Some(b'<'), _, _) => (TokenKind::LShift, 2),
            (b'>', Some(b'>'), _, _) => (TokenKind::RShift, 2),
            (b'+', Some(b'+'), _, _) => (TokenKind::PlusPlus, 2),
            (b'-', Some(b'-'), _, _) => (TokenKind::MinusMinus, 2),
            (b'*', Some(b'*'), _, _) => (TokenKind::StarStar, 2),
            (b'&', Some(b'&'), _, _) => (TokenKind::AmpAmp, 2),
            (b'|', Some(b'|'), _, _) => (TokenKind::PipePipe, 2),
            (b'?', Some(b'?'), _, _) => (TokenKind::QuestionQuestion, 2),
            // `?.` is optional chaining ONLY if not followed by an ASCII digit (`x?.5:1`).
            (b'?', Some(b'.'), next, _) if !next.is_some_and(|n| n.is_ascii_digit()) => {
                (TokenKind::QuestionDot, 2)
            }
            (b'+', Some(b'='), _, _) => (TokenKind::PlusEq, 2),
            (b'-', Some(b'='), _, _) => (TokenKind::MinusEq, 2),
            (b'*', Some(b'='), _, _) => (TokenKind::StarEq, 2),
            (b'/', Some(b'='), _, _) => (TokenKind::SlashEq, 2),
            (b'%', Some(b'='), _, _) => (TokenKind::PercentEq, 2),
            (b'&', Some(b'='), _, _) => (TokenKind::AmpEq, 2),
            (b'|', Some(b'='), _, _) => (TokenKind::PipeEq, 2),
            (b'^', Some(b'='), _, _) => (TokenKind::CaretEq, 2),
            (b'(', _, _, _) => (TokenKind::LParen, 1),
            (b')', _, _, _) => (TokenKind::RParen, 1),
            (b'{', _, _, _) => (TokenKind::LBrace, 1),
            (b'}', _, _, _) => (TokenKind::RBrace, 1),
            (b'[', _, _, _) => (TokenKind::LBracket, 1),
            (b']', _, _, _) => (TokenKind::RBracket, 1),
            (b'.', _, _, _) => (TokenKind::Dot, 1),
            (b';', _, _, _) => (TokenKind::Semicolon, 1),
            (b',', _, _, _) => (TokenKind::Comma, 1),
            (b':', _, _, _) => (TokenKind::Colon, 1),
            (b'?', _, _, _) => (TokenKind::Question, 1),
            (b'+', _, _, _) => (TokenKind::Plus, 1),
            (b'-', _, _, _) => (TokenKind::Minus, 1),
            (b'*', _, _, _) => (TokenKind::Star, 1),
            (b'/', _, _, _) => (TokenKind::Slash, 1),
            (b'%', _, _, _) => (TokenKind::Percent, 1),
            (b'!', _, _, _) => (TokenKind::Bang, 1),
            (b'~', _, _, _) => (TokenKind::Tilde, 1),
            (b'<', _, _, _) => (TokenKind::Lt, 1),
            (b'>', _, _, _) => (TokenKind::Gt, 1),
            (b'&', _, _, _) => (TokenKind::Amp, 1),
            (b'|', _, _, _) => (TokenKind::Pipe, 1),
            (b'^', _, _, _) => (TokenKind::Caret, 1),
            (b'=', _, _, _) => (TokenKind::Eq, 1),
            _ => {
                let ch = self.peek_char().unwrap_or('\u{FFFD}');
                self.pos += ch.len_utf8();
                return Err(SyntaxError::new(
                    format!("Unexpected token '{ch}'"),
                    Span::new(start, self.pos as u32),
                ));
            }
        };

        self.pos += len;
        Ok(kind)
    }
}

fn push_normalized_raw(dst: &mut String, s: &str) {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            dst.push('\n');
        } else {
            dst.push(c);
        }
    }
}

#[inline]
fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

#[inline]
fn is_whitespace(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\u{000B}' | '\u{000C}' | '\u{00A0}' | '\u{FEFF}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

#[inline]
fn is_ascii_id_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'$' || b == b'_'
}

#[inline]
fn is_ascii_id_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'$' || b == b'_'
}

#[inline]
fn is_id_continue(ch: char) -> bool {
    ch == '$' || ch == '_' || ch == '\u{200C}' || ch == '\u{200D}' || is_xid_continue(ch)
}

#[inline]
fn hex_val(b: u8) -> Option<u32> {
    match b {
        b'0'..=b'9' => Some(u32::from(b - b'0')),
        b'a'..=b'f' => Some(u32::from(b - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(b - b'A' + 10)),
        _ => None,
    }
}
