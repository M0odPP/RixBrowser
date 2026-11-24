//! Pure-Rust UAX #14 + W3C CSS Text Level 3 §5.1–§5.3 line breaking engine.
//!
//! Backed by ICU4X (`icu::segmenter::LineSegmenter`) with W3C CSS Text L3
//! `line-break` (`auto | loose | normal | strict | anywhere`), `word-break`
//! (`normal | break-all | keep-all | break-word`), and HTML `lang` (`ja` / `zh`)
//! tailoring.

use icu::locale::LanguageIdentifier;
use icu::properties::props::EastAsianWidth;
use icu::properties::CodePointMapData;
use icu::segmenter::options::{LineBreakOptions, LineBreakStrictness, LineBreakWordOption};
use icu::segmenter::LineSegmenter;
use rix_css::{ComputedStyle, LineBreakMode, WordBreakMode};

/// Parses an HTML `lang` / `xml:lang` attribute into an ICU4X [`LanguageIdentifier`].
/// Falls back to the primary BCP 47 language subtag if the full tag is non-canonical.
#[must_use]
pub fn parse_lang_id(lang: Option<&str>) -> Option<LanguageIdentifier> {
    let raw = lang?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(id) = raw.parse::<LanguageIdentifier>() {
        return Some(id);
    }
    let primary = raw.split(['-', '_']).next()?;
    primary.parse::<LanguageIdentifier>().ok()
}

/// Returns `true` if `lang` identifies Japanese (`ja`) or Chinese (`zh`).
#[must_use]
pub fn is_ja_or_zh(lang: Option<&str>) -> bool {
    let Some(raw) = lang.map(str::trim) else {
        return false;
    };
    let primary = raw.split(['-', '_']).next().unwrap_or("");
    primary.eq_ignore_ascii_case("ja") || primary.eq_ignore_ascii_case("zh")
}

/// Returns all valid soft-wrap byte offsets inside `text` (strictly in `1..text.len()`)
/// according to UAX #14 + W3C CSS Text Level 3 §5.2–§5.3 (`line-break`, `word-break`, `lang`).
#[must_use]
pub fn line_break_offsets(text: &str, style: &ComputedStyle, lang: Option<&str>) -> Vec<usize> {
    if text.len() <= 1 {
        return Vec::new();
    }

    let strictness = match style.text.line_break {
        LineBreakMode::Auto | LineBreakMode::Normal => LineBreakStrictness::Normal,
        LineBreakMode::Loose => LineBreakStrictness::Loose,
        LineBreakMode::Strict => LineBreakStrictness::Strict,
        LineBreakMode::Anywhere => LineBreakStrictness::Anywhere,
    };
    let word_option = match style.text.word_break {
        WordBreakMode::BreakAll => LineBreakWordOption::BreakAll,
        WordBreakMode::KeepAll => LineBreakWordOption::KeepAll,
        WordBreakMode::Normal
        | WordBreakMode::BreakWord
        | WordBreakMode::Manual
        | WordBreakMode::AutoPhrase => LineBreakWordOption::Normal,
    };

    let lang_id = parse_lang_id(lang);
    let mut opts = LineBreakOptions::default();
    opts.strictness = Some(strictness);
    opts.word_option = Some(word_option);
    opts.content_locale = lang_id.as_ref();

    let segmenter = LineSegmenter::new_auto(opts);
    let mut breaks: Vec<usize> = segmenter
        .segment_str(text)
        .filter(|&idx| idx > 0 && idx < text.len())
        .collect();

    // Apply exact W3C CSS Text L3 §5.3 tailoring adjustments where Unicode UAX #14
    // defaults differ from CSS Text L3 normative rules.
    if strictness != LineBreakStrictness::Anywhere && word_option != LineBreakWordOption::BreakAll {
        let ja_zh = is_ja_or_zh(lang);
        apply_css_text_l3_tailoring(text, strictness, ja_zh, &mut breaks);
    }

    breaks
}

/// Returns `true` if a soft-wrap opportunity exists between `prev_char` and `next_char`
/// under `style` and `lang`.
#[must_use]
pub fn can_break_between(
    prev_char: char,
    next_char: char,
    style: &ComputedStyle,
    lang: Option<&str>,
) -> bool {
    let mut buf = [0u8; 8];
    let p_len = prev_char.encode_utf8(&mut buf[..4]).len();
    let n_len = next_char.encode_utf8(&mut buf[p_len..]).len();
    let pair = std::str::from_utf8(&buf[..p_len + n_len]).unwrap_or("");
    line_break_offsets(pair, style, lang).contains(&p_len)
}

/// W3C CSS Text L3 §5.3 `line-break` tailoring table over UAX #14 break offsets.
fn apply_css_text_l3_tailoring(
    text: &str,
    strictness: LineBreakStrictness,
    ja_zh: bool,
    breaks: &mut Vec<usize>,
) {
    let mut char_positions: Vec<(usize, char)> = text.char_indices().collect();
    if char_positions.len() < 2 {
        return;
    }

    // Track current breaks in a sorted Vec and adjust per CSS Text L3 §5.3.
    for i in 1..char_positions.len() {
        let (byte_idx, next_ch) = char_positions[i];
        let (_, prev_ch) = char_positions[i - 1];

        // Never override glue/word-joiner/whitespace prohibitions.
        if matches!(
            prev_ch,
            '\u{00A0}' | '\u{202F}' | '\u{2060}' | '\u{FEFF}' | '\u{200D}'
        ) || matches!(
            next_ch,
            '\u{00A0}' | '\u{202F}' | '\u{2060}' | '\u{FEFF}' | '\u{200D}'
        ) {
            continue;
        }

        let override_break: Option<bool> = if is_conditional_japanese_starter(next_ch) {
            // CSS Text L3 §5.3: breaks before small kana (CJ) forbidden in `strict`,
            // allowed in `normal` and `loose` (when preceded by breakable/CJK context).
            if prev_ch.is_ascii_alphanumeric() {
                None
            } else {
                Some(strictness != LineBreakStrictness::Strict)
            }
        } else if is_iteration_mark(next_ch) {
            // CSS Text L3 §5.3: breaks before iteration marks allowed in `loose`,
            // forbidden in `normal` and `strict`.
            Some(strictness == LineBreakStrictness::Loose)
        } else if is_inseparable_char(next_ch) {
            // CSS Text L3 §5.3: breaks between/before inseparables allowed in `loose`,
            // forbidden in `normal` and `strict`.
            Some(strictness == LineBreakStrictness::Loose)
        } else if is_japanese_hyphen(next_ch) {
            // CSS Text L3 §5.3: breaks before Japanese hyphens (‐ – 〜 ゠) allowed in
            // `loose`/`normal` for ja/zh, forbidden in `strict` or non-ja/zh.
            Some(ja_zh && strictness != LineBreakStrictness::Strict)
        } else if is_centered_punctuation(next_ch) {
            // CSS Text L3 §5.3: breaks before centered punctuation allowed ONLY in
            // `loose` for ja/zh.
            Some(ja_zh && strictness == LineBreakStrictness::Loose)
        } else if is_postfix_eaw_afw(next_ch) {
            // CSS Text L3 §5.3: breaks before PO (East Asian Width A/F/W) allowed ONLY
            // in `loose` for ja/zh.
            Some(ja_zh && strictness == LineBreakStrictness::Loose)
        } else if is_prefix_eaw_afw(next_ch) {
            // CSS Text L3 §5.3: breaks before PR (East Asian Width A/F/W):
            // - For ja/zh: allowed in `loose`, forbidden in `normal`/`strict`.
            // - For non-ja/zh: allowed (when preceded by CJK ideograph).
            if ja_zh {
                Some(strictness == LineBreakStrictness::Loose)
            } else if is_cjk_ideograph(prev_ch) {
                Some(true)
            } else {
                None
            }
        } else {
            None
        };

        if let Some(allow) = override_break {
            match breaks.binary_search(&byte_idx) {
                Ok(pos) if !allow => {
                    breaks.remove(pos);
                }
                Err(pos) if allow => {
                    breaks.insert(pos, byte_idx);
                }
                _ => {}
            }
        }
    }
    let _ = &mut char_positions;
}

/// Conditional Japanese Starters (UAX #14 class CJ / CSS Text L3 §5.3).
const fn is_conditional_japanese_starter(c: char) -> bool {
    matches!(
        c,
        '\u{3041}'
            | '\u{3043}'
            | '\u{3045}'
            | '\u{3047}'
            | '\u{3049}'
            | '\u{3063}'
            | '\u{3083}'
            | '\u{3085}'
            | '\u{3087}'
            | '\u{308E}'
            | '\u{3095}'
            | '\u{3096}'
            | '\u{30A1}'
            | '\u{30A3}'
            | '\u{30A5}'
            | '\u{30A7}'
            | '\u{30A9}'
            | '\u{30C3}'
            | '\u{30E3}'
            | '\u{30E5}'
            | '\u{30E7}'
            | '\u{30EE}'
            | '\u{30F5}'
            | '\u{30F6}'
            | '\u{30FC}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{FF67}'..='\u{FF70}'
            | '\u{1B132}'
            | '\u{1B150}'..='\u{1B152}'
            | '\u{1B155}'
            | '\u{1B164}'..='\u{1B167}'
    )
}

/// Iteration marks (CSS Text L3 §5.3).
const fn is_iteration_mark(c: char) -> bool {
    matches!(
        c,
        '\u{3005}' | '\u{303B}' | '\u{309D}' | '\u{309E}' | '\u{30FD}' | '\u{30FE}'
    )
}

/// Inseparable characters (UAX #14 class IN / CSS Text L3 §5.3).
const fn is_inseparable_char(c: char) -> bool {
    matches!(
        c,
        '\u{2024}' | '\u{2025}' | '\u{2026}' | '\u{2014}' | '\u{22EF}' | '\u{FE19}'
    )
}

/// Japanese hyphens (CSS Text L3 §5.3).
const fn is_japanese_hyphen(c: char) -> bool {
    matches!(c, '\u{2010}' | '\u{2013}' | '\u{301C}' | '\u{30A0}')
}

/// Centered punctuation marks (CSS Text L3 §5.3).
const fn is_centered_punctuation(c: char) -> bool {
    matches!(
        c,
        '\u{30FB}'
            | '\u{FF1A}'
            | '\u{FF1B}'
            | '\u{FF65}'
            | '\u{203C}'
            | '\u{2047}'
            | '\u{2048}'
            | '\u{2049}'
            | '\u{0021}'
            | '\u{003F}'
            | '\u{FF01}'
            | '\u{FF1F}'
    ) && !matches!(c, '\u{0021}' | '\u{003F}')
}

/// Postfix numeric characters with East Asian Width A/F/W (CSS Text L3 §5.3).
const fn is_postfix_eaw_afw(c: char) -> bool {
    matches!(
        c,
        '\u{00B0}'
            | '\u{2030}'
            | '\u{2032}'
            | '\u{2033}'
            | '\u{2035}'
            | '\u{2103}'
            | '\u{2109}'
            | '\u{FE6A}'
            | '\u{FF05}'
            | '\u{FFE0}'
    )
}

/// Prefix numeric characters with East Asian Width A/F/W (CSS Text L3 §5.3).
const fn is_prefix_eaw_afw(c: char) -> bool {
    matches!(
        c,
        '\u{00B1}'
            | '\u{20AC}'
            | '\u{2116}'
            | '\u{FE69}'
            | '\u{FF04}'
            | '\u{FFE1}'
            | '\u{FFE5}'
            | '\u{FFE6}'
    )
}

const fn is_cjk_ideograph(c: char) -> bool {
    matches!(
        c,
        '\u{3040}'..='\u{30FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2FA1F}'
    )
}

/// Returns `true` for Unicode bidi formatting control characters ignored during
/// CSS Text L3/L4 §4.1.1 whitespace collapsing around segment breaks.
#[inline]
#[must_use]
pub const fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Returns `true` for Unicode default-ignorable zero-width format/control characters
/// that must not render as visible tofu boxes or add horizontal advance during shaping.
#[inline]
#[must_use]
pub const fn is_zero_width_ignorable(c: char) -> bool {
    is_bidi_control(c)
        || matches!(
            c,
            '\u{00AD}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
        )
}

/// Returns `true` for Hangul syllables, Jamo, and halfwidth Hangul Jamo.
#[inline]
const fn is_hangul(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7FF}'
            | '\u{FFA0}'..='\u{FFDC}'
    )
}

/// Returns `true` if `c` has Unicode East Asian Width `F`, `W`, or `H` and is NOT Hangul
/// (CSS Text L4 §4.1.2 Segment Break Transformation Rules).
#[inline]
fn is_eaw_fwh_non_hangul(c: char) -> bool {
    if is_hangul(c) {
        return false;
    }
    matches!(
        CodePointMapData::<EastAsianWidth>::new().get(c),
        EastAsianWidth::Fullwidth | EastAsianWidth::Wide | EastAsianWidth::Halfwidth
    )
}

/// Returns `true` if a collapsible segment break between `before` and `after`
/// (ignoring intervening bidi controls) should be removed instead of replaced
/// by a space per CSS Text L4 §4.1.2.
#[must_use]
pub fn should_remove_segment_break(before: Option<char>, after: Option<char>) -> bool {
    if before == Some('\u{200B}') || after == Some('\u{200B}') {
        return true;
    }
    match (before, after) {
        (Some(b), Some(a)) => is_eaw_fwh_non_hangul(b) && is_eaw_fwh_non_hangul(a),
        _ => false,
    }
}

/// Transforms collapsible ASCII whitespace and segment breaks in `text` per
/// CSS Text Level 3/4 §4.1.1 & §4.1.2:
/// - Normalizes `\r\n` / `\r` to `\n`.
/// - Removes ASCII spaces/tabs immediately preceding or following a segment break
///   while ignoring Unicode bidi control characters.
/// - When `!preserve_breaks`, removes segment breaks adjacent to `U+200B` ZWSP or
///   between non-Hangul East Asian Width `F`/`W`/`H` characters, and converts all
///   other segment breaks to `' '`.
/// - Collapses consecutive ASCII spaces/tabs (ignoring intervening bidi controls)
///   into a single `' '` (or discards all ASCII whitespace when `discard_ws`).
#[must_use]
pub fn collapse_whitespace_l4(
    text: &str,
    preserve_breaks: bool,
    discard_ws: bool,
    prev_non_bidi: Option<char>,
    next_non_bidi: Option<char>,
    prev_ended_with_space: bool,
) -> Vec<String> {
    // Normalize CRLF and bare CR into LF.
    let mut chars: Vec<char> = Vec::with_capacity(text.len());
    let mut iter = text.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\r' {
            if iter.peek() == Some(&'\n') {
                iter.next();
            }
            chars.push('\n');
        } else {
            chars.push(c);
        }
    }

    let is_sp_tab = |c: char| matches!(c, ' ' | '\t' | '\x0C');
    let n = chars.len();

    // Pass 1: mark spaces/tabs adjacent to segment breaks (ignoring bidi controls) for removal.
    let mut removed = vec![false; n];
    for i in 0..n {
        if chars[i] == '\n' {
            let mut j = i;
            while j > 0 {
                j -= 1;
                if is_bidi_control(chars[j]) {
                    continue;
                }
                if is_sp_tab(chars[j]) {
                    removed[j] = true;
                } else {
                    break;
                }
            }
            let mut k = i + 1;
            while k < n {
                if is_bidi_control(chars[k]) {
                    k += 1;
                    continue;
                }
                if is_sp_tab(chars[k]) {
                    removed[k] = true;
                    k += 1;
                } else {
                    break;
                }
            }
        }
    }

    // Pass 2: build output lines, transforming segment breaks and collapsing spaces
    // (ignoring intervening bidi control characters).
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::with_capacity(text.len());
    let mut in_space = prev_ended_with_space;
    let mut i = 0usize;
    while i < n {
        if removed[i] {
            i += 1;
            continue;
        }
        let c = chars[i];
        if c == '\n' {
            if preserve_breaks {
                lines.push(std::mem::take(&mut cur));
                in_space = false;
                i += 1;
                continue;
            }
            // Collapse consecutive segment breaks (and intervening bidi controls / removed spaces)
            // as a single segment-break sequence.
            let mut end = i + 1;
            let mut held_bidi = Vec::new();
            while end < n {
                if removed[end] || chars[end] == '\n' {
                    end += 1;
                } else if is_bidi_control(chars[end]) {
                    held_bidi.push(chars[end]);
                    end += 1;
                } else {
                    break;
                }
            }
            // Find last non-bidi char before `i` (falling back to `prev_non_bidi`).
            let before = (0..i)
                .rev()
                .find(|&idx| !removed[idx] && chars[idx] != '\n' && !is_bidi_control(chars[idx]))
                .map(|idx| chars[idx])
                .or(prev_non_bidi);
            // Find first non-bidi char at/after `end` (falling back to `next_non_bidi`).
            let after = (end..n)
                .find(|&idx| !removed[idx] && chars[idx] != '\n' && !is_bidi_control(chars[idx]))
                .map(|idx| chars[idx])
                .or(next_non_bidi);

            if !discard_ws && !should_remove_segment_break(before, after) && !in_space {
                cur.push(' ');
                in_space = true;
            }
            for bc in held_bidi {
                cur.push(bc);
            }
            i = end;
            continue;
        }
        if is_sp_tab(c) {
            if !discard_ws && !in_space {
                cur.push(' ');
                in_space = true;
            }
            i += 1;
            continue;
        }
        if is_bidi_control(c) {
            // Bidi controls pass through without resetting `in_space`.
            cur.push(c);
            i += 1;
            continue;
        }
        cur.push(c);
        in_space = false;
        i += 1;
    }
    lines.push(cur);
    lines
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn style_with_lb(lb: LineBreakMode) -> ComputedStyle {
        let mut s = ComputedStyle::default();
        Arc::make_mut(&mut s.text).line_break = lb;
        s
    }

    #[test]
    fn css_text_l3_strictness_matrix_matches_spec() {
        // CJ (small kana ぁ U+3041): allowed in loose/normal, forbidden in strict.
        assert!(can_break_between(
            '文',
            '\u{3041}',
            &style_with_lb(LineBreakMode::Loose),
            Some("ja")
        ));
        assert!(can_break_between(
            '文',
            '\u{3041}',
            &style_with_lb(LineBreakMode::Normal),
            Some("ja")
        ));
        assert!(!can_break_between(
            '文',
            '\u{3041}',
            &style_with_lb(LineBreakMode::Strict),
            Some("ja")
        ));

        // Iteration mark (々 U+3005): allowed in loose, forbidden in normal/strict.
        assert!(can_break_between(
            '文',
            '\u{3005}',
            &style_with_lb(LineBreakMode::Loose),
            None
        ));
        assert!(!can_break_between(
            '文',
            '\u{3005}',
            &style_with_lb(LineBreakMode::Normal),
            None
        ));
        assert!(!can_break_between(
            '文',
            '\u{3005}',
            &style_with_lb(LineBreakMode::Strict),
            None
        ));

        // Centered punctuation (・ U+30FB): allowed in loose ONLY for ja/zh.
        assert!(can_break_between(
            '文',
            '\u{30FB}',
            &style_with_lb(LineBreakMode::Loose),
            Some("ja")
        ));
        assert!(!can_break_between(
            '文',
            '\u{30FB}',
            &style_with_lb(LineBreakMode::Loose),
            Some("de")
        ));
        assert!(!can_break_between(
            '文',
            '\u{30FB}',
            &style_with_lb(LineBreakMode::Normal),
            Some("ja")
        ));

        // Prefix currency (€ U+20AC): for ja/zh forbidden in normal/strict; for unknown allowed.
        assert!(!can_break_between(
            '文',
            '\u{20AC}',
            &style_with_lb(LineBreakMode::Normal),
            Some("ja")
        ));
        assert!(can_break_between(
            '文',
            '\u{20AC}',
            &style_with_lb(LineBreakMode::Normal),
            None
        ));
    }

    #[test]
    fn uax14_ba_zw_gl_wj_rules_conform() {
        let normal = style_with_lb(LineBreakMode::Normal);
        // U+0964 DEVANAGARI DANDA (BA): break after.
        let s = "aaa\u{0964}bbb";
        let offs = line_break_offsets(s, &normal, Some("en"));
        assert!(offs.contains(&(3 + '\u{0964}'.len_utf8())));
        // U+00A0 NBSP (GL) and U+2060 WJ: no break.
        assert!(line_break_offsets("aaa\u{00A0}bbb", &normal, Some("en")).is_empty());
        assert!(line_break_offsets("aaa\u{2060}bbb", &normal, Some("en")).is_empty());
        // U+200B ZWSP (ZW): break after.
        let zw = "aaa\u{200B}bbb";
        assert!(line_break_offsets(zw, &normal, Some("en")).contains(&(3 + '\u{200B}'.len_utf8())));
    }
}
