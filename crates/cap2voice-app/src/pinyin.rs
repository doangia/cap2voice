//! Chinese-to-pinyin conversion for the overlay pinyin line (D2).
//!
//! Pure Rust via the `pinyin` crate (tone-marked syllables), so the feature
//! survives D1 (which removes the Python server). Polyphones use the
//! crate's most common reading (no context disambiguation) - a documented
//! limitation, not a bug. Non-Chinese tokens (punctuation, digits, Latin)
//! pass through unchanged.

use pinyin::ToPinyin;

/// CJK punctuation that attaches to the preceding syllable without a space
/// (the space goes after it, before the next word).
const CJK_PUNCTUATION: &[char] = &[
    '\u{3002}', // 。
    '\u{FF0C}', // ，
    '\u{3001}', // 、
    '\u{FF1B}', // ；
    '\u{FF1A}', // ：
    '\u{FF1F}', // ？
    '\u{FF01}', // ！
    '\u{2026}', // …
    '\u{00B7}', // ·
    '\u{300C}', // 「
    '\u{300D}', // 」
    '\u{300E}', // 『
    '\u{300F}', // 』
    '\u{FF08}', // （
    '\u{FF09}', // ）
    '\u{300A}', // 《
    '\u{300B}', // 》
    '\u{3008}', // 〈
    '\u{3009}', // 〉
    '\u{3010}', //
    '\u{2014}', // —
    '\u{FF5E}', // ～
];

/// One renderable piece of a pinyin line: a word (a tone-marked syllable or
/// a passed-through Latin/digit run) or a punctuation mark that attaches
/// directly to the preceding word.
enum Token {
    Word(String),
    Punct(char),
}

/// Convert a Chinese subtitle line to tone-marked pinyin: one syllable per
/// Chinese character, syllables separated by single spaces, CJK punctuation
/// kept as-is and attached to the preceding syllable. Non-Chinese runs
/// (digits, Latin) pass through unchanged as their own words.
///
/// Examples: "你好，我是小明。" -> "nǐ hǎo， wǒ shì xiǎo míng。",
/// "2024年" -> "2024 nián", "" -> "".
pub fn to_pinyin_line(chinese: &str) -> String {
    let mut tokens: Vec<Token> = Vec::new();
    let mut latin_run = String::new();
    for ch in chinese.chars() {
        if ch.is_whitespace() {
            if !latin_run.is_empty() {
                tokens.push(Token::Word(std::mem::take(&mut latin_run)));
            }
            continue;
        }
        if CJK_PUNCTUATION.contains(&ch) {
            if !latin_run.is_empty() {
                tokens.push(Token::Word(std::mem::take(&mut latin_run)));
            }
            tokens.push(Token::Punct(ch));
            continue;
        }
        match ch.to_pinyin() {
            Some(py) => {
                if !latin_run.is_empty() {
                    tokens.push(Token::Word(std::mem::take(&mut latin_run)));
                }
                tokens.push(Token::Word(py.with_tone().to_string()));
            }
            None => {
                latin_run.push(ch);
            }
        }
    }
    if !latin_run.is_empty() {
        tokens.push(Token::Word(latin_run));
    }

    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Word(word) => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&word);
            }
            Token::Punct(punct) => {
                out.push(punct);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::to_pinyin_line;

    #[test]
    fn pinyin_line_splits_and_marks_tones() {
        assert_eq!(
            to_pinyin_line("你好，我是小明。"),
            "nǐ hǎo， wǒ shì xiǎo míng。"
        );
        assert_eq!(to_pinyin_line("你好。"), "nǐ hǎo。");
        assert_eq!(to_pinyin_line("再见。"), "zài jiàn。");
        assert_eq!(to_pinyin_line("是"), "shì");
    }

    #[test]
    fn pinyin_line_passes_through_non_chinese() {
        assert_eq!(to_pinyin_line(""), "");
        assert_eq!(to_pinyin_line("Hello"), "Hello");
        assert_eq!(to_pinyin_line("2024年"), "2024 nián");
        assert_eq!(to_pinyin_line("你好！再见"), "nǐ hǎo！ zài jiàn");
    }

    #[test]
    fn pinyin_line_uses_most_common_reading_for_polyphones() {
        // 行 has several readings (xíng/háng/...); the crate returns the
        // most common one. No context disambiguation is attempted.
        assert_eq!(to_pinyin_line("行"), "xíng");
    }
}
