use rix_js::lexer::{Keyword, Lexer, TokenKind};

fn lex_all(src: &str) -> Vec<TokenKind> {
    let mut lexer = Lexer::new(src);
    let mut out = Vec::new();
    loop {
        let tok = lexer.next_token().expect("lex should succeed");
        if tok.kind == TokenKind::Eof {
            break;
        }
        out.push(tok.kind);
    }
    out
}

#[test]
fn unicode_identifiers_escapes_private_names_and_hashbang() {
    let src =
        "#!/usr/bin/env rix-js\nconst \u{03B1} = 1; let caf\\u00E9 = \\u{1F600}; #secret; i\\u0066";
    let mut lexer = Lexer::new(src);
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Keyword(Keyword::Const)
    );
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Ident {
            name: "\u{03B1}".into(),
            escaped: false,
        }
    );
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Eq);
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Number {
            value: 1.0,
            legacy_octal: false,
        }
    );
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Semicolon);
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Ident {
            name: "let".into(),
            escaped: false,
        }
    );
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Ident {
            name: "caf\u{00E9}".into(),
            escaped: true,
        }
    );
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Eq);
    // `\u{1F600}` is an emoji (not ID_Start), so lexing it as an identifier must fail.
    assert!(lexer.next_token().is_err());

    // Escaped keyword `i\u0066` must lex as Ident { escaped: true }, not Keyword::If.
    let tokens = lex_all("#secret; i\\u0066");
    assert_eq!(
        tokens,
        vec![
            TokenKind::PrivateIdent("secret".into()),
            TokenKind::Semicolon,
            TokenKind::Ident {
                name: "if".into(),
                escaped: true,
            },
        ]
    );
}

#[test]
fn numeric_separators_radices_bigint_and_legacy_octal() {
    let tokens =
        lex_all("1_000_000 0xFF_00 0b1010_0101 0o7_7 3.14_15e+2 9007199254740993n 0x10n 077");
    assert_eq!(
        tokens,
        vec![
            TokenKind::Number {
                value: 1_000_000.0,
                legacy_octal: false
            },
            TokenKind::Number {
                value: 0xFF00 as f64,
                legacy_octal: false
            },
            TokenKind::Number {
                value: 0b1010_0101 as f64,
                legacy_octal: false
            },
            TokenKind::Number {
                value: 63.0,
                legacy_octal: false
            },
            TokenKind::Number {
                value: 314.15,
                legacy_octal: false
            },
            TokenKind::BigInt("9007199254740993".into()),
            TokenKind::BigInt("0x10".into()),
            TokenKind::Number {
                value: 63.0,
                legacy_octal: true
            },
        ]
    );

    // Invalid numeric forms must be rejected.
    assert!(Lexer::new("1__0").next_token().is_err());
    assert!(Lexer::new("1_").next_token().is_err());
    assert!(Lexer::new("1.5n").next_token().is_err());
    assert!(Lexer::new("077n").next_token().is_err());
    assert!(Lexer::new("123abc").next_token().is_err());
}

#[test]
fn template_literals_strings_and_regex_validation() {
    let src = "`hello ${name}, count=${1 + 2}!`";
    let mut lexer = Lexer::new(src);
    let head = lexer.next_token().unwrap();
    assert_eq!(
        head.kind,
        TokenKind::TemplateHead {
            raw: "hello ".into(),
            cooked: Some("hello ".into()),
        }
    );
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Ident {
            name: "name".into(),
            escaped: false,
        }
    );
    let rbrace1 = lexer.next_token().unwrap();
    assert_eq!(rbrace1.kind, TokenKind::RBrace);
    let mid = lexer
        .lex_template_continuation(rbrace1.span.end as usize)
        .unwrap();
    assert_eq!(
        mid.kind,
        TokenKind::TemplateMiddle {
            raw: ", count=".into(),
            cooked: Some(", count=".into()),
        }
    );
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Number {
            value: 1.0,
            legacy_octal: false
        }
    );
    assert_eq!(lexer.next_token().unwrap().kind, TokenKind::Plus);
    assert_eq!(
        lexer.next_token().unwrap().kind,
        TokenKind::Number {
            value: 2.0,
            legacy_octal: false
        }
    );
    let rbrace2 = lexer.next_token().unwrap();
    assert_eq!(rbrace2.kind, TokenKind::RBrace);
    let tail = lexer
        .lex_template_continuation(rbrace2.span.end as usize)
        .unwrap();
    assert_eq!(
        tail.kind,
        TokenKind::TemplateTail {
            raw: "!".into(),
            cooked: Some("!".into()),
        }
    );

    // Regex literal with character class containing `/` and ES2024 flags.
    let mut rlex = Lexer::new("/foo[a/b]bar\\//giv");
    let slash = rlex.next_token().unwrap();
    assert_eq!(slash.kind, TokenKind::Slash);
    let (pattern, flags, _) = rlex.lex_regex_literal(slash.span.start as usize).unwrap();
    assert_eq!(pattern, "foo[a/b]bar\\/");
    assert_eq!(flags, "giv");

    // Reject duplicate flags and mutually exclusive `u`+`v` flags.
    let mut bad1 = Lexer::new("/a/gg");
    let s1 = bad1.next_token().unwrap();
    assert!(bad1.lex_regex_literal(s1.span.start as usize).is_err());

    let mut bad2 = Lexer::new("/a/uv");
    let s2 = bad2.next_token().unwrap();
    assert!(bad2.lex_regex_literal(s2.span.start as usize).is_err());
}

#[test]
fn punctuators_optional_chaining_vs_ternary_and_asi_newlines() {
    // `x?.5:1` must lex `?` then `.5`, NOT `?.`!
    let tokens = lex_all("x?.5:1; a?.b ??= c &&= d ||= e >>>= 1");
    assert_eq!(
        tokens,
        vec![
            TokenKind::Ident {
                name: "x".into(),
                escaped: false
            },
            TokenKind::Question,
            TokenKind::Number {
                value: 0.5,
                legacy_octal: false
            },
            TokenKind::Colon,
            TokenKind::Number {
                value: 1.0,
                legacy_octal: false
            },
            TokenKind::Semicolon,
            TokenKind::Ident {
                name: "a".into(),
                escaped: false
            },
            TokenKind::QuestionDot,
            TokenKind::Ident {
                name: "b".into(),
                escaped: false
            },
            TokenKind::QuestionQuestionEq,
            TokenKind::Ident {
                name: "c".into(),
                escaped: false
            },
            TokenKind::AmpAmpEq,
            TokenKind::Ident {
                name: "d".into(),
                escaped: false
            },
            TokenKind::PipePipeEq,
            TokenKind::Ident {
                name: "e".into(),
                escaped: false
            },
            TokenKind::URShiftEq,
            TokenKind::Number {
                value: 1.0,
                legacy_octal: false
            },
        ]
    );

    // Multiline block comment must set `line_terminator_before = true` for ASI.
    let mut lexer = Lexer::new("return /* line1\nline2 */ 42");
    let t1 = lexer.next_token().unwrap();
    assert!(!t1.line_terminator_before);
    let t2 = lexer.next_token().unwrap();
    assert!(t2.line_terminator_before);
}
