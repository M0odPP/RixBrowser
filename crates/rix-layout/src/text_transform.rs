//! Pure-Rust W3C CSS Text Level 3/4 + MathML Core `text-transform` engine.
//!
//! Powered by ICU4X (`icu::casemap::CaseMapper`) for full locale-sensitive Unicode
//! case mappings (`uppercase`, `lowercase`, `capitalize`), plus CSS Text L3
//! `full-width`, CSS Text L3 Appendix G `full-size-kana`, and MathML Core
//! §3.2.1.1 `math-auto`.

use icu::casemap::options::{LeadingAdjustment, TitlecaseOptions, TrailingCase};
use icu::casemap::CaseMapper;
use icu::locale::LanguageIdentifier;
use rix_css::TextTransformFlags;

use crate::line_break::parse_lang_id;

/// Applies full CSS Text L3/L4 + MathML Core `text-transform` flags to `text`.
///
/// `prev_char` provides the character immediately preceding `text` in the same
/// inline formatting context so `capitalize` respects word boundaries across
/// inline element boundaries (e.g., `a<span style="text-transform: capitalize">b</span>c`).
#[must_use]
pub fn apply_text_transform_flags(
    text: &str,
    flags: TextTransformFlags,
    lang: Option<&str>,
    prev_char: Option<char>,
) -> String {
    if flags.is_none() || text.is_empty() {
        return text.to_string();
    }
    if flags.is_math_auto() {
        return text.chars().map(map_math_auto).collect();
    }

    let lang_id = parse_lang_id(lang).unwrap_or(LanguageIdentifier::UNKNOWN);
    let cm = CaseMapper::new();

    // 1. Case transform (uppercase / lowercase / capitalize).
    let mut cur = match flags.case_bits() {
        TextTransformFlags::UPPERCASE => cm.uppercase_to_string(text, &lang_id).into_owned(),
        TextTransformFlags::LOWERCASE => cm.lowercase_to_string(text, &lang_id).into_owned(),
        TextTransformFlags::CAPITALIZE => capitalize_css(text, &lang_id, prev_char),
        _ => text.to_string(),
    };

    // 2. `full-width` (CSS Text L3 §2.1).
    if flags.is_full_width() {
        cur = cur.chars().map(map_full_width).collect();
    }

    // 3. `full-size-kana` (CSS Text L3 Appendix G).
    if flags.is_full_size_kana() {
        cur = cur.chars().map(map_full_size_kana).collect();
    }

    cur
}

/// W3C CSS Text L3 §2.1 `text-transform: capitalize`:
/// Converts the first typographic letter unit of each word to titlecase without
/// lowercasing subsequent letters (preserving e.g. `iPhone` -> `IPhone`,
/// `foo_bar` -> `Foo_bar`, `john's` -> `John's`, `foo-bar` -> `Foo-Bar`,
/// and keeping Georgian Mkhedruli unchanged per Unicode 11 titlecase rules).
fn capitalize_css(text: &str, lang_id: &LanguageIdentifier, prev_char: Option<char>) -> String {
    let cm = CaseMapper::new();
    let mut opts = TitlecaseOptions::default();
    opts.leading_adjustment = Some(LeadingAdjustment::None);
    opts.trailing_case = Some(TrailingCase::Unchanged);

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_word = prev_char.is_some_and(is_word_interior_char);

    for (i, &ch) in chars.iter().enumerate() {
        if is_word_letter_or_digit(ch) {
            if !in_word {
                let mut buf = [0u8; 4];
                let s = ch.encode_utf8(&mut buf);
                // Handle Dutch `ij` digraph when lang is `nl` and next char is `j`/`J`.
                if lang_id.language.as_str() == "nl"
                    && matches!(ch, 'i' | 'I')
                    && chars.get(i + 1).is_some_and(|&n| matches!(n, 'j' | 'J'))
                {
                    out.push('I');
                } else {
                    out.push_str(
                        &cm.titlecase_segment_with_only_case_data_to_string(s, lang_id, opts),
                    );
                }
            } else if lang_id.language.as_str() == "nl"
                && i == 1
                && matches!(chars[0], 'i' | 'I')
                && matches!(ch, 'j' | 'J')
                && !prev_char.is_some_and(is_word_interior_char)
            {
                out.push('J');
            } else {
                out.push(ch);
            }
            in_word = true;
        } else if ch == '_' {
            // Underscore is a Unicode word-connector (`Pc`), keeping words unbroken (`foo_bar`).
            out.push(ch);
            in_word = true;
        } else if matches!(ch, '\'' | '\u{2019}')
            && in_word
            && chars
                .get(i + 1)
                .is_some_and(|&next| is_word_letter_or_digit(next))
        {
            // Mid-word apostrophe (UAX #29 MidNumLet / Single_Quote, e.g. `john's`).
            out.push(ch);
        } else {
            out.push(ch);
            in_word = false;
        }
    }
    out
}

fn is_word_letter_or_digit(c: char) -> bool {
    c.is_alphanumeric()
}

fn is_word_interior_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// MathML Core §3.2.1.1 italic math alphanumeric mapping (`text-transform: math-auto`).
#[must_use]
pub const fn map_math_auto(c: char) -> char {
    let cp = c as u32;
    let mapped = match cp {
        0x0041..=0x005A => 0x1D434 + (cp - 0x0041),
        0x0068 => 0x210E,
        0x0061..=0x007A => 0x1D44E + (cp - 0x0061),
        0x0131 => 0x1D6A4,
        0x0237 => 0x1D6A5,
        0x0391..=0x03A1 => 0x1D6E2 + (cp - 0x0391),
        0x03F4 => 0x1D6F3,
        0x03A3..=0x03A9 => 0x1D6F4 + (cp - 0x03A3),
        0x2207 => 0x1D6FB,
        0x03B1..=0x03C9 => 0x1D6FC + (cp - 0x03B1),
        0x2202 => 0x1D715,
        0x03F5 => 0x1D716,
        0x03D1 => 0x1D717,
        0x03F0 => 0x1D718,
        0x03D5 => 0x1D719,
        0x03F1 => 0x1D71A,
        0x03D6 => 0x1D71B,
        _ => return c,
    };
    match char::from_u32(mapped) {
        Some(ch) => ch,
        None => c,
    }
}

/// CSS Text Level 3 §2.1 `full-width` character mapping.
#[must_use]
pub const fn map_full_width(c: char) -> char {
    let cp = c as u32;
    let mapped = match cp {
        0x0020 => 0x3000,
        0x0021..=0x007E => 0xFF01 + (cp - 0x0021),
        0x00A2 => 0xFFE0,
        0x00A3 => 0xFFE1,
        0x00AC => 0xFFE2,
        0x00AF => 0xFFE3,
        0x00A6 => 0xFFE4,
        0x00A5 => 0xFFE5,
        0x20A9 => 0xFFE6,
        0x2985 => 0xFF5F,
        0x2986 => 0xFF60,
        0xFF61 => 0x3002,
        0xFF62 => 0x300C,
        0xFF63 => 0x300D,
        0xFF64 => 0x3001,
        0xFF65 => 0x30FB,
        0xFF66 => 0x30F2,
        0xFF67 => 0x30A1,
        0xFF68 => 0x30A3,
        0xFF69 => 0x30A5,
        0xFF6A => 0x30A7,
        0xFF6B => 0x30A9,
        0xFF6C => 0x30E3,
        0xFF6D => 0x30E5,
        0xFF6E => 0x30E7,
        0xFF6F => 0x30C3,
        0xFF70 => 0x30FC,
        0xFF71 => 0x30A2,
        0xFF72 => 0x30A4,
        0xFF73 => 0x30A6,
        0xFF74 => 0x30A8,
        0xFF75 => 0x30AA,
        0xFF76 => 0x30AB,
        0xFF77 => 0x30AD,
        0xFF78 => 0x30AF,
        0xFF79 => 0x30B1,
        0xFF7A => 0x30B3,
        0xFF7B => 0x30B5,
        0xFF7C => 0x30B7,
        0xFF7D => 0x30B9,
        0xFF7E => 0x30BB,
        0xFF7F => 0x30BD,
        0xFF80 => 0x30BF,
        0xFF81 => 0x30C1,
        0xFF82 => 0x30C4,
        0xFF83 => 0x30C6,
        0xFF84 => 0x30C8,
        0xFF85 => 0x30CA,
        0xFF86 => 0x30CB,
        0xFF87 => 0x30CC,
        0xFF88 => 0x30CD,
        0xFF89 => 0x30CE,
        0xFF8A => 0x30CF,
        0xFF8B => 0x30D2,
        0xFF8C => 0x30D5,
        0xFF8D => 0x30D8,
        0xFF8E => 0x30DB,
        0xFF8F => 0x30DE,
        0xFF90 => 0x30DF,
        0xFF91 => 0x30E0,
        0xFF92 => 0x30E1,
        0xFF93 => 0x30E2,
        0xFF94 => 0x30E4,
        0xFF95 => 0x30E6,
        0xFF96 => 0x30E8,
        0xFF97 => 0x30E9,
        0xFF98 => 0x30EA,
        0xFF99 => 0x30EB,
        0xFF9A => 0x30EC,
        0xFF9B => 0x30ED,
        0xFF9C => 0x30EF,
        0xFF9D => 0x30F3,
        0xFF9E => 0x3099,
        0xFF9F => 0x309A,
        _ => return c,
    };
    match char::from_u32(mapped) {
        Some(ch) => ch,
        None => c,
    }
}

/// CSS Text Level 3 Appendix G Small Kana Mappings (`text-transform: full-size-kana`).
#[must_use]
pub const fn map_full_size_kana(c: char) -> char {
    match c {
        // Small Hiragana
        '\u{3041}' => '\u{3042}',
        '\u{3043}' => '\u{3044}',
        '\u{3045}' => '\u{3046}',
        '\u{3047}' => '\u{3048}',
        '\u{3049}' => '\u{304A}',
        '\u{3063}' => '\u{3064}',
        '\u{3083}' => '\u{3084}',
        '\u{3085}' => '\u{3086}',
        '\u{3087}' => '\u{3088}',
        '\u{308E}' => '\u{308F}',
        '\u{3095}' => '\u{304B}',
        '\u{3096}' => '\u{3051}',
        // Small Katakana
        '\u{30A1}' => '\u{30A2}',
        '\u{30A3}' => '\u{30A4}',
        '\u{30A5}' => '\u{30A6}',
        '\u{30A7}' => '\u{30A8}',
        '\u{30A9}' => '\u{30AA}',
        '\u{30C3}' => '\u{30C4}',
        '\u{30E3}' => '\u{30E4}',
        '\u{30E5}' => '\u{30E6}',
        '\u{30E7}' => '\u{30E8}',
        '\u{30EE}' => '\u{30EF}',
        '\u{30F5}' => '\u{30AB}',
        '\u{30F6}' => '\u{30B1}',
        // Katakana Phonetic Extensions
        '\u{31F0}' => '\u{30AF}',
        '\u{31F1}' => '\u{30B7}',
        '\u{31F2}' => '\u{30B9}',
        '\u{31F3}' => '\u{30C8}',
        '\u{31F4}' => '\u{30CC}',
        '\u{31F5}' => '\u{30CF}',
        '\u{31F6}' => '\u{30D2}',
        '\u{31F7}' => '\u{30D5}',
        '\u{31F8}' => '\u{30D8}',
        '\u{31F9}' => '\u{30DB}',
        '\u{31FA}' => '\u{30E0}',
        '\u{31FB}' => '\u{30E9}',
        '\u{31FC}' => '\u{30EA}',
        '\u{31FD}' => '\u{30EB}',
        '\u{31FE}' => '\u{30EC}',
        '\u{31FF}' => '\u{30ED}',
        // Small Kana Extension (Unicode 12+)
        '\u{1B132}' => '\u{3053}',
        '\u{1B150}' => '\u{3090}',
        '\u{1B151}' => '\u{3091}',
        '\u{1B152}' => '\u{3092}',
        '\u{1B155}' => '\u{30B3}',
        '\u{1B164}' => '\u{30F0}',
        '\u{1B165}' => '\u{30F1}',
        '\u{1B166}' => '\u{30F2}',
        '\u{1B167}' => '\u{30F3}',
        // Halfwidth Small Katakana
        '\u{FF67}' => '\u{FF71}',
        '\u{FF68}' => '\u{FF72}',
        '\u{FF69}' => '\u{FF73}',
        '\u{FF6A}' => '\u{FF74}',
        '\u{FF6B}' => '\u{FF75}',
        '\u{FF6C}' => '\u{FF94}',
        '\u{FF6D}' => '\u{FF95}',
        '\u{FF6E}' => '\u{FF96}',
        '\u{FF6F}' => '\u{FF82}',
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uppercase_lowercase_capitalize_german_and_georgian() {
        assert_eq!(
            apply_text_transform_flags(
                "ß",
                TextTransformFlags(TextTransformFlags::UPPERCASE),
                Some("de"),
                None
            ),
            "SS"
        );
        // Georgian Mkhedruli <-> Mtavruli
        assert_eq!(
            apply_text_transform_flags(
                "ა",
                TextTransformFlags(TextTransformFlags::UPPERCASE),
                Some("ka"),
                None
            ),
            "Ა"
        );
        assert_eq!(
            apply_text_transform_flags(
                "Ა",
                TextTransformFlags(TextTransformFlags::LOWERCASE),
                Some("ka"),
                None
            ),
            "ა"
        );
        assert_eq!(
            apply_text_transform_flags(
                "ა ბ",
                TextTransformFlags(TextTransformFlags::CAPITALIZE),
                Some("ka"),
                None
            ),
            "ა ბ"
        );
        // Capitalize word boundaries
        let cap = TextTransformFlags(TextTransformFlags::CAPITALIZE);
        assert_eq!(
            apply_text_transform_flags("foo-bar", cap, None, None),
            "Foo-Bar"
        );
        assert_eq!(
            apply_text_transform_flags("john's apple", cap, None, None),
            "John's Apple"
        );
        assert_eq!(
            apply_text_transform_flags("foo_bar", cap, None, None),
            "Foo_bar"
        );
        assert_eq!(apply_text_transform_flags("b", cap, None, Some('a')), "b");
    }

    #[test]
    fn math_auto_and_full_size_kana_mappings() {
        assert_eq!(
            apply_text_transform_flags(
                "h",
                TextTransformFlags(TextTransformFlags::MATH_AUTO),
                None,
                None
            ),
            "\u{210E}"
        );
        assert_eq!(
            apply_text_transform_flags(
                "A",
                TextTransformFlags(TextTransformFlags::MATH_AUTO),
                None,
                None
            ),
            "\u{1D434}"
        );
        assert_eq!(
            apply_text_transform_flags(
                "\u{3041}\u{1B167}\u{FF6F}",
                TextTransformFlags(TextTransformFlags::FULL_SIZE_KANA),
                None,
                None
            ),
            "\u{3042}\u{30F3}\u{FF82}"
        );
    }
}
