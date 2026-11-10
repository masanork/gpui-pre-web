// Modified by ibis inc. for ibisOffice: Japanese documents' full-width
// letters, digits and symbols (Ｕ＋FF01–FF60, ※, ①, ○, → and the like) are
// eligible for the browser-font fallback, as the bundled fonts lack them.
// Also modified by ibis inc.: Latin graphemes (`is_latin_grapheme`) and the
// separator in a font's fallback list (`LATIN_FALLBACK_SEPARATOR`) let the
// text system draw half-width letters and digits with the typeface a document
// names when the browser has it installed.

//! Eligibility only for independent, horizontal Canvas fallback of a complete
//! extended grapheme. Callers must still check whether bundled shaping supplies
//! the requested glyph and presentation; eligibility alone does not override it.
//!
//! This is intentionally not a Unicode sequence validator or an RGI emoji database.
//! Han variation selectors are checked structurally, not against the IVD registry.
//! Emoji selectors are checked against the Emoji property, not the registered
//! variation-sequence list. Eligibility does not guarantee browser coverage.
//! Flags accept two regional indicators, not only assigned country codes. ZWJ
//! support is limited to person/profession pairs and a small explicit allowlist;
//! tags are limited to the three subdivision flags. Other sequences stay on Cosmic.

use unicode_properties::{EmojiStatus, GeneralCategory, UnicodeEmoji, UnicodeGeneralCategory};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

/// Controls browser-font fallback when loaded fonts lack a glyph or its emoji presentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CanvasFontFallback {
    /// Use only fonts loaded into GPUI.
    Disabled,
    /// Use Canvas for eligible emoji, including missing text-presentation symbols.
    #[default]
    Emoji,
    /// Also allow approximate independent rendering of eligible horizontal CJK text.
    EmojiAndCjk,
}

impl CanvasFontFallback {
    #[cfg(any(target_family = "wasm", test))]
    pub(crate) fn allows(self, is_emoji: bool) -> bool {
        match self {
            Self::Disabled => false,
            Self::Emoji => is_emoji,
            Self::EmojiAndCjk => true,
        }
    }
}

/// An entry of a font's fallback list that ends the typefaces a document asks
/// for (and their aliases). What follows it are stand-ins for East Asian
/// text, which Latin text must not be drawn with.
pub const LATIN_FALLBACK_SEPARATOR: &str = "";

/// The typeface names of `fallbacks` that Latin text may be drawn with: the
/// ones before the first [`LATIN_FALLBACK_SEPARATOR`] (all of them if there is
/// none).
pub fn requested_family_names(fallbacks: &[String]) -> impl Iterator<Item = &str> {
    fallbacks
        .iter()
        .map(String::as_str)
        .take_while(|name| *name != LATIN_FALLBACK_SEPARATOR)
}

/// Whether `grapheme` is one printable ASCII character or one Latin-script
/// letter: text a typeface for Latin draws, which the bundled fonts also have.
pub fn is_latin_grapheme(grapheme: &str) -> bool {
    let mut characters = grapheme.chars();
    let Some(base) = characters.next() else {
        return false;
    };
    if characters.next().is_some() {
        return false;
    }
    if base.is_ascii() {
        return !base.is_ascii_control();
    }
    // Latin-1 letters, Latin Extended-A and -B, Latin Extended Additional
    // (not the full-width forms, which have the Latin script too).
    matches!(base, '\u{c0}'..='\u{24f}' | '\u{1e00}'..='\u{1eff}')
        && !matches!(base, '\u{d7}' | '\u{f7}')
        && matches!(
            base.general_category(),
            GeneralCategory::UppercaseLetter | GeneralCategory::LowercaseLetter
        )
}

/// A supported grapheme assumed to be independently renderable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanvasFallback {
    /// Whether this is an emoji-capable grapheme, independent of its presentation.
    pub is_emoji: bool,
    /// Whether to request emoji presentation instead of ordinary text presentation.
    pub emoji_presentation: bool,
}

/// Returns a presentation only when `grapheme` is exactly one supported extended
/// grapheme cluster. No normalization, splitting, or font-coverage decision occurs.
pub fn classify_canvas_fallback(grapheme: &str) -> Option<CanvasFallback> {
    if grapheme.is_ascii() {
        return None;
    }
    let mut graphemes = grapheme.graphemes(true);
    if graphemes.next()? != grapheme || graphemes.next().is_some() {
        return None;
    }

    if is_cjk(grapheme) {
        Some(CanvasFallback {
            is_emoji: false,
            emoji_presentation: false,
        })
    } else {
        classify_emoji(grapheme).map(|emoji_presentation| CanvasFallback {
            is_emoji: true,
            emoji_presentation,
        })
    }
}

fn is_cjk(grapheme: &str) -> bool {
    let mut characters = grapheme.chars();
    let Some(base) = characters.next() else {
        return false;
    };
    let suffix = characters.as_str();

    // The script/category checks exclude unassigned holes; the ranges exclude
    // Han radicals, iteration marks, and other non-ideographic Han characters.
    if base.script() == Script::Han
        && base.general_category() == GeneralCategory::OtherLetter
        && matches!(base, '\u{3400}'..='\u{9fff}' | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{323af}')
    {
        return suffix.is_empty()
            || matches!(
                (characters.next(), characters.next()),
                (Some('\u{fe00}'..='\u{fe02}' | '\u{e0100}'..='\u{e01ef}'), None)
            );
    }

    if matches!(base, '\u{3041}'..='\u{3096}' | '\u{30a1}'..='\u{30fa}') {
        return suffix.is_empty()
            || match suffix {
                "\u{3099}" => "うかきくけこさしすせそたちつてとはひふへほウカキクケコサシスセソタチツテトハヒフヘホワヰヱヲ".contains(base),
                "\u{309a}" => "はひふへほハヒフヘホ".contains(base),
                _ => false,
            };
    }

    if matches!(base, '\u{ac00}'..='\u{d7a3}') {
        return suffix.is_empty()
            || ((base as u32 - 0xac00).is_multiple_of(28)
                && is_single_modern_trailing_jamo(suffix));
    }
    if matches!(base, '\u{1100}'..='\u{1112}') {
        return matches!(characters.next(), Some('\u{1161}'..='\u{1175}'))
            && (characters.as_str().is_empty()
                || is_single_modern_trailing_jamo(characters.as_str()));
    }

    suffix.is_empty()
        && (matches!(base, '\u{3131}'..='\u{3163}')
            || "、。〈〉《》「」『』【】〔〕（）［］｛｝，．！？：；・ー々〆".contains(base)
            || is_japanese_symbol(base))
}

/// Symbols Japanese text uses that the bundled Latin fonts do not have:
/// CJK punctuation, full-width and half-width forms, and the dashes,
/// marks, arrows, shapes and enclosed numbers of JIS X 0208 and 0213.
fn is_japanese_symbol(c: char) -> bool {
    matches!(c,
        '\u{3000}'..='\u{303f}'   // CJK symbols and punctuation
        | '\u{3097}'..='\u{309f}' // kana marks
        | '\u{30fb}'..='\u{30ff}' // katakana middle dot, marks
        | '\u{31f0}'..='\u{31ff}' // katakana phonetic extensions
        | '\u{ff01}'..='\u{ff9f}' // full-width forms, half-width katakana
        | '\u{ffe0}'..='\u{ffe6}' // full-width signs (￥ ￡ ￢ …)
        | '\u{2010}'..='\u{2027}' // dashes, quotes, ‥ …
        | '\u{2030}'..='\u{203b}' // ‰ ′ ″ ※
        | '\u{2103}' | '\u{2116}' | '\u{2121}' | '\u{212b}' // ℃ № ℡ Å
        | '\u{2160}'..='\u{217f}' // Roman numerals
        | '\u{2190}'..='\u{21ff}' // arrows
        | '\u{2200}'..='\u{22ff}' // mathematical operators
        | '\u{2460}'..='\u{24ff}' // enclosed numbers (①)
        | '\u{2500}'..='\u{257f}' // box drawing
        | '\u{25a0}'..='\u{25ff}' // geometric shapes (□ ■ ○ ●)
        | '\u{2605}' | '\u{2606}' | '\u{2640}' | '\u{2642}' // ★ ☆ ♀ ♂
        | '\u{3200}'..='\u{33ff}' // enclosed CJK, CJK compatibility (㈱ ㎡)
        | '\u{fe10}'..='\u{fe19}' // vertical punctuation presentation forms
        | '\u{fe30}'..='\u{fe44}' | '\u{fe47}'..='\u{fe48}' // vertical brackets and leaders
    )
}

fn is_single_modern_trailing_jamo(text: &str) -> bool {
    let mut characters = text.chars();
    matches!(characters.next(), Some('\u{11a8}'..='\u{11c2}')) && characters.next().is_none()
}

fn classify_emoji(grapheme: &str) -> Option<bool> {
    let mut characters = grapheme.chars();
    let base = characters.next()?;
    let suffix = characters.as_str();

    if matches!(base, '0'..='9' | '#' | '*') {
        return match suffix {
            "\u{20e3}" | "\u{fe0f}\u{20e3}" => Some(true),
            _ => None,
        };
    }
    if unicode_properties::emoji::is_regional_indicator(base) {
        return (characters
            .next()
            .is_some_and(unicode_properties::emoji::is_regional_indicator)
            && characters.next().is_none())
        .then_some(true);
    }
    if matches!(
        grapheme,
        "🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}"
            | "🏴\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}"
            | "🏴\u{e0067}\u{e0062}\u{e0077}\u{e006c}\u{e0073}\u{e007f}"
    ) {
        return Some(true);
    }
    if grapheme.contains('\u{200d}') {
        return is_supported_zwj_sequence(grapheme).then_some(true);
    }
    classify_emoji_unit(grapheme)
}

fn classify_emoji_unit(text: &str) -> Option<bool> {
    let mut characters = text.chars();
    let base = characters.next()?;
    if !base.is_emoji_char() || base.is_emoji_component() {
        return None;
    }
    let status = base.emoji_status();
    let mut emoji_presentation = matches!(
        status,
        EmojiStatus::EmojiPresentation | EmojiStatus::EmojiPresentationAndModifierBase
    );
    let mut next = characters.next();
    let explicit_text = next == Some('\u{fe0e}');
    if matches!(next, Some('\u{fe0e}' | '\u{fe0f}')) {
        emoji_presentation = !explicit_text;
        next = characters.next();
    }
    if let Some(modifier) = next {
        if explicit_text
            || !matches!(
                status,
                EmojiStatus::EmojiModifierBase | EmojiStatus::EmojiPresentationAndModifierBase
            )
            || modifier.emoji_status() != EmojiStatus::EmojiPresentationAndModifierAndEmojiComponent
        {
            return None;
        }
        emoji_presentation = true;
    }
    characters.next().is_none().then_some(emoji_presentation)
}

fn is_supported_zwj_sequence(grapheme: &str) -> bool {
    if matches!(
        grapheme,
        "👨‍👩‍👧" | "👨‍👩‍👧‍👦"
            | "👩‍👩‍👧‍👦"
            | "👨‍👨‍👧‍👦"
            | "🏳️‍🌈"
            | "🏳️‍⚧️"
            | "🏴‍☠️"
            | "❤️‍🔥"
            | "❤️‍🩹"
            | "👁️‍🗨️"
            | "🐻‍❄️"
    ) {
        return true;
    }
    let Some((person, profession)) = grapheme.split_once('\u{200d}') else {
        return false;
    };
    matches!(person.chars().next(), Some('👨' | '👩' | '🧑'))
        && classify_emoji_unit(person) == Some(true)
        && matches!(
            profession,
            "⚕️" | "⚖️"
                | "✈️"
                | "🌾"
                | "🍳"
                | "🎓"
                | "🎤"
                | "🎨"
                | "🏫"
                | "🏭"
                | "💻"
                | "💼"
                | "🔧"
                | "🔬"
                | "🚀"
                | "🚒"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_symbols_fall_back() {
        for c in ["５", "※", "％", "＋", "①", "□", "●", "→", "―", "…", "㈱", "ｱ", "￥", "　", "︑", "︒", "︵", "︶", "﹁", "﹂", "︙"] {
            assert!(is_cjk(c), "{c}");
        }
        assert!(!is_cjk("A"));
        assert!(!is_cjk("5"));
    }

    #[test]
    fn canvas_font_fallback_policy() {
        assert_eq!(CanvasFontFallback::default(), CanvasFontFallback::Emoji);
        for (grapheme, allowed) in [
            ("😀", true),
            ("❤️", true),
            ("1️⃣", true),
            ("👨‍👩‍👧‍👦", true),
            ("🕸", true),
            ("🕸\u{fe0e}", true),
            ("🕸\u{fe0f}", true),
            ("©", true),
            ("❤︎", true),
            ("中", false),
            ("か\u{3099}", false),
            ("각", false),
        ] {
            let fallback = classify_canvas_fallback(grapheme).expect("eligible grapheme");
            assert!(!CanvasFontFallback::Disabled.allows(fallback.is_emoji));
            assert_eq!(
                CanvasFontFallback::Emoji.allows(fallback.is_emoji),
                allowed,
                "{grapheme:?}",
            );
            assert!(CanvasFontFallback::EmojiAndCjk.allows(fallback.is_emoji));
        }
    }

    #[test]
    fn ascii_is_ineligible_but_keycaps_are_preserved() {
        for byte in 0..=0x7f_u8 {
            assert_eq!(classify_canvas_fallback(&(byte as char).to_string()), None);
        }
        for text in ["", "Hello", "0123456789#*", "\r\n"] {
            assert_eq!(classify_canvas_fallback(text), None);
        }
        for base in "0123456789#*".chars() {
            for text in [format!("{base}\u{20e3}"), format!("{base}\u{fe0f}\u{20e3}")] {
                assert_eq!(
                    classify_canvas_fallback(&text),
                    Some(CanvasFallback {
                        is_emoji: true,
                        emoji_presentation: true,
                    }),
                    "{text:?}"
                );
            }
        }
    }

    #[test]
    fn ordinary_cjk_clusters() {
        for text in [
            "漢",
            "𠀀",
            "﨑",
            "漢\u{fe00}",
            "葛\u{e0100}",
            "葛\u{e01ef}",
            "あ",
            "ガ",
            "か\u{3099}",
            "ハ\u{309a}",
            "가",
            "각",
            "가",
            "각",
            "각",
            "ㄱ",
            "、",
            "。",
            "「",
            "」",
            "（",
            "！",
            "ー",
            "々",
        ] {
            assert_eq!(
                classify_canvas_fallback(text),
                Some(CanvasFallback {
                    is_emoji: false,
                    emoji_presentation: false,
                }),
                "{text:?}"
            );
        }
    }

    #[test]
    fn emoji_presentation_is_preserved() {
        for (text, emoji_presentation) in [
            ("😀", true),
            ("🕸", false),
            ("🕸\u{fe0e}", false),
            ("🕸\u{fe0f}", true),
            ("©", false),
            ("©\u{fe0e}", false),
            ("©\u{fe0f}", true),
            ("❤", false),
            ("❤\u{fe0e}", false),
            ("❤\u{fe0f}", true),
            ("😀\u{fe0e}", false),
            ("👍🏽", true),
            ("☝🏽", true),
            ("👍\u{fe0f}🏽", true),
            ("🇯🇵", true),
            ("1\u{20e3}", true),
            ("#\u{fe0f}\u{20e3}", true),
            ("*\u{fe0f}\u{20e3}", true),
            ("👩🏽‍💻", true),
            ("🧑‍⚕️", true),
            ("👨‍👩‍👧‍👦", true),
            ("🏳️‍🌈", true),
            (
                "🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}",
                true,
            ),
            (
                "🏴\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}",
                true,
            ),
            (
                "🏴\u{e0067}\u{e0062}\u{e0077}\u{e006c}\u{e0073}\u{e007f}",
                true,
            ),
        ] {
            assert_eq!(
                classify_canvas_fallback(text),
                Some(CanvasFallback {
                    is_emoji: true,
                    emoji_presentation,
                }),
                "{text:?}"
            );
        }
    }

    #[test]
    fn unsupported_scripts_and_cjk_forms_stay_on_cosmic() {
        for text in [
            "a",
            "é",
            "α",
            "Ж",
            "ش",
            "ש",
            "क",
            "क्ष",
            "ก",
            "ក",
            "ᠠ",
            "ཀ",
            "\0",
            "\n",
            "\r\n",
            "\u{202e}",
            "\u{200d}",
            "\u{fdd0}",
            "\u{10ffff}",
            "\u{2a6e0}",
            "\u{e000}",
            "\u{3099}",
            "\u{fe0f}",
            "\u{e0100}",
            "漢\u{301}",
            "漢\u{fe03}",
            "漢\u{fe0f}",
            "漢\u{e0100}\u{e0101}",
            "あ\u{3099}",
            "か\u{309a}",
            "か\u{3099}\u{3099}",
            "ᄀ",
            "ᅡ",
            "ᆨ",
            "ᄓᅡ",
            "ᄀᅶ",
            "가ᇃ",
            "각ᆨ",
            "ᄀ가",
            "\u{3164}",
            "\u{3165}",
            "⺀",
            "⼀",
            "㇀",
            "㆐",
            "ㄅ",
            "㐀\u{200d}",
            "\u{1b000}",
            "ｶ",
            "！\u{301}",
        ] {
            assert_eq!(classify_canvas_fallback(text), None, "{text:?}");
        }
    }

    #[test]
    fn malformed_or_out_of_policy_emoji_stay_on_cosmic() {
        for text in [
            "0",
            "#",
            "*",
            "1\u{fe0f}",
            "#\u{fe0e}",
            "1\u{fe0e}\u{20e3}",
            "a\u{20e3}",
            "🏽",
            "🇯",
            "😀🏽",
            "👍🏽🏽",
            "👍\u{fe0e}🏽",
            "👍🏽\u{fe0f}",
            "❤\u{fe0f}\u{fe0f}",
            "😀\u{301}",
            "🦰",
            "😀‍😀",
            "👩‍",
            "👩‍💻‍🚀",
            "👩\u{fe0e}‍💻",
            "🏴\u{e0067}",
            "🏴\u{e0061}\u{e0062}\u{e007f}",
            "😀\u{e0067}\u{e007f}",
        ] {
            assert_eq!(classify_canvas_fallback(text), None, "{text:?}");
        }
    }

    #[test]
    fn accepts_only_one_whole_extended_grapheme() {
        for text in [
            "",
            "漢字",
            "あい",
            "가나",
            "😀😀",
            "🇯🇵🇺",
            "🇯🇵🇺🇸",
            " 漢",
            "漢\n",
        ] {
            assert_eq!(classify_canvas_fallback(text), None, "{text:?}");
        }
        let text = "aか\u{3099}👩🏽‍💻漢\u{e0100}";
        let eligible: Vec<_> = text
            .grapheme_indices(true)
            .filter_map(|(start, grapheme)| {
                classify_canvas_fallback(grapheme).map(|_| start..start + grapheme.len())
            })
            .collect();
        assert_eq!(
            eligible
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["か\u{3099}", "👩🏽‍💻", "漢\u{e0100}"]
        );
    }

    #[test]
    fn latin_graphemes_are_single_ascii_or_latin_letters() {
        for text in ["a", "Z", "0", " ", "(", "~", "é", "Ł"] {
            assert!(is_latin_grapheme(text), "{text:?}");
        }
        for text in ["", "ab", "\t", "\u{7f}", "あ", "Ａ", "１", "e\u{301}", "😀", "ж"] {
            assert!(!is_latin_grapheme(text), "{text:?}");
        }
    }

    #[test]
    fn requested_names_stop_at_the_separator() {
        let list: Vec<String> = ["MS Mincho", "Times New Roman", "", "Hiragino Sans"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            requested_family_names(&list).collect::<Vec<_>>(),
            ["MS Mincho", "Times New Roman"]
        );
        let none: Vec<String> = ["Arial"].map(String::from).to_vec();
        assert_eq!(requested_family_names(&none).collect::<Vec<_>>(), ["Arial"]);
    }
}
