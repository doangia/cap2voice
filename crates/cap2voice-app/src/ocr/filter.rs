//! OCR text block ordering, CJK character filtering, and fuzzy deduplication.

use wcocr_rs::TextBlock;

/// Default normalized Levenshtein similarity threshold at or above which
/// a consecutive Chinese subtitle is treated as a duplicate and suppressed.
pub const DEFAULT_DEDUPE_THRESHOLD: f32 = 0.93;

/// Default number of consecutive blank/non-CJK OCR frames required to clear
/// the previous-subtitle memory in `FuzzyDeduplicator`.
pub const DEFAULT_BLANK_RESET_FRAMES: u32 = 5;

/// Return `true` when `c` is a CJK Unified Ideograph (or Compatibility Ideograph).
pub fn is_cjk_ideograph(c: char) -> bool {
    matches!(
        c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF
    )
}

fn is_retained_subtitle_char(c: char) -> bool {
    let code = c as u32;
    is_cjk_ideograph(c)
        || matches!(code, 0x3000..=0x303F)
        || matches!(
            code,
            0xFF01..=0xFF20 | 0xFF3B..=0xFF40 | 0xFF5B..=0xFF65 | 0xFFE0..=0xFFEF
        )
        || matches!(
            c,
            '\u{2018}'
                | '\u{2019}'
                | '\u{201C}'
                | '\u{201D}'
                | '\u{2014}'
                | '\u{2026}'
                | '\u{00B7}'
        )
        || c.is_ascii_digit()
        || matches!(
            c,
            '.' | ',' | '!' | '?' | ';' | ':' | '\'' | '"' | '-' | '(' | ')'
        )
}

/// Sort OCR `TextBlock`s into natural reading order: top-to-bottom across lines,
/// grouping blocks whose vertical centers fall within half a line height, and
/// left-to-right within each line.
pub fn sort_text_blocks(blocks: &[TextBlock]) -> Vec<&TextBlock> {
    if blocks.is_empty() {
        return Vec::new();
    }

    let mut items: Vec<(&TextBlock, f32, f32)> = blocks
        .iter()
        .map(|b| {
            let y_center = (b.top + b.bottom) * 0.5;
            let height = (b.bottom - b.top).abs().max(1.0);
            (b, y_center, height)
        })
        .collect();

    items.sort_by(|a, b| {
        a.1.total_cmp(&b.1)
            .then_with(|| a.0.left.total_cmp(&b.0.left))
    });

    struct LineGroup<'a> {
        blocks: Vec<&'a TextBlock>,
        sum_y: f32,
        sum_h: f32,
    }

    let mut lines: Vec<LineGroup<'_>> = Vec::new();
    for (block, y_center, height) in items {
        let belongs_to_last = if let Some(last) = lines.last() {
            let count = last.blocks.len() as f32;
            let avg_y = last.sum_y / count;
            let avg_h = last.sum_h / count;
            (y_center - avg_y).abs() <= 0.5 * avg_h.max(height)
        } else {
            false
        };

        if belongs_to_last {
            if let Some(last) = lines.last_mut() {
                last.blocks.push(block);
                last.sum_y += y_center;
                last.sum_h += height;
            }
        } else {
            lines.push(LineGroup {
                blocks: vec![block],
                sum_y: y_center,
                sum_h: height,
            });
        }
    }

    let mut sorted = Vec::with_capacity(blocks.len());
    for mut line in lines {
        line.blocks.sort_by(|a, b| {
            a.left
                .total_cmp(&b.left)
                .then_with(|| a.top.total_cmp(&b.top))
        });
        sorted.extend(line.blocks);
    }
    sorted
}

/// Sort `blocks` into reading order and join non-empty trimmed block texts with spaces.
pub fn join_sorted_blocks(blocks: &[TextBlock]) -> String {
    sort_text_blocks(blocks)
        .into_iter()
        .map(|b| b.text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Filter `raw` text to retain Chinese characters, digits, and punctuation while
/// dropping Latin noise. Returns `None` when the result contains zero CJK ideographs.
pub fn filter_cjk_text(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut has_cjk = false;

    for c in raw.chars() {
        if c.is_whitespace() {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else if is_retained_subtitle_char(c) {
            if is_cjk_ideograph(c) {
                has_cjk = true;
            }
            out.push(c);
        }
    }

    while out.ends_with(' ') {
        out.pop();
    }

    has_cjk.then_some(out)
}

/// Sort `blocks` in reading order, join them, and filter to Chinese subtitle text.
pub fn extract_chinese_subtitle(blocks: &[TextBlock]) -> Option<String> {
    let joined = join_sorted_blocks(blocks);
    filter_cjk_text(&joined)
}

/// Sort `blocks` in reading order and filter each block's text separately,
/// returning one non-empty Chinese line per visual line (top-to-bottom).
/// Two visual lines in one OCR frame stay separate here so the pipeline can
/// treat them as two utterances (A2 / gh #1) instead of one merged line.
/// Returns an empty `Vec` when no block contains CJK text.
pub fn extract_chinese_subtitle_lines(blocks: &[TextBlock]) -> Vec<String> {
    sort_text_blocks(blocks)
        .into_iter()
        .filter_map(|block| filter_cjk_text(block.text.trim()))
        .collect()
}

/// Strip whitespace and punctuation, leaving only CJK ideographs and normalized digits
/// for deduplication comparison.
pub fn normalize_for_comparison(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_cjk_ideograph(c) || c.is_ascii_digit() {
            out.push(c);
        } else if ('\u{FF10}'..='\u{FF19}').contains(&c) {
            if let Some(digit) = char::from_digit(c as u32 - 0xFF10, 10) {
                out.push(digit);
            }
        }
    }
    out
}

/// Compute Unicode scalar (`char`) Levenshtein edit distance between `a` and `b`.
pub fn levenshtein_chars(a: &[char], b: &[char]) -> usize {
    let (longer, shorter) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    if shorter.is_empty() {
        return longer.len();
    }

    let mut prev: Vec<usize> = (0..=shorter.len()).collect();
    let mut curr = vec![0usize; shorter.len() + 1];

    for (i, &ca) in longer.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in shorter.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[shorter.len()]
}

/// Return normalized character similarity in `[0.0, 1.0]`:
/// `1.0 - (edit_distance / max(len_a, len_b))`, or `1.0` when both strings are empty.
pub fn normalized_similarity(a: &str, b: &str) -> f32 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein_chars(&a_chars, &b_chars);
    1.0 - (dist as f32 / max_len as f32)
}

/// Stateful subtitle deduplicator interface.
pub trait TextDeduplicator: Send {
    fn process(&mut self, filtered_text: Option<&str>) -> Option<String>;
    fn reset(&mut self);
}

/// Normalized-Levenshtein fuzzy subtitle deduplicator with blank-frame reset.
#[derive(Debug, Clone)]
pub struct FuzzyDeduplicator {
    threshold: f32,
    blank_reset_frames: u32,
    last_normalized: Option<String>,
    consecutive_blanks: u32,
}

impl Default for FuzzyDeduplicator {
    fn default() -> Self {
        Self::new(DEFAULT_DEDUPE_THRESHOLD, DEFAULT_BLANK_RESET_FRAMES)
    }
}

impl FuzzyDeduplicator {
    pub fn new(threshold: f32, blank_reset_frames: u32) -> Self {
        Self {
            threshold,
            blank_reset_frames: blank_reset_frames.max(1),
            last_normalized: None,
            consecutive_blanks: 0,
        }
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn blank_reset_frames(&self) -> u32 {
        self.blank_reset_frames
    }

    pub fn set_threshold(&mut self, threshold: f32) {
        self.threshold = threshold;
    }

    pub fn set_blank_reset_frames(&mut self, frames: u32) {
        self.blank_reset_frames = frames.max(1);
    }
}

impl TextDeduplicator for FuzzyDeduplicator {
    fn process(&mut self, filtered_text: Option<&str>) -> Option<String> {
        let normalized = filtered_text
            .map(normalize_for_comparison)
            .filter(|s| !s.is_empty());

        let Some(norm) = normalized else {
            self.consecutive_blanks = self.consecutive_blanks.saturating_add(1);
            if self.consecutive_blanks >= self.blank_reset_frames {
                self.last_normalized = None;
            }
            return None;
        };

        self.consecutive_blanks = 0;
        if let Some(prev) = &self.last_normalized {
            if normalized_similarity(&norm, prev) >= self.threshold {
                return None;
            }
        }

        self.last_normalized = Some(norm);
        filtered_text.map(str::to_owned)
    }

    fn reset(&mut self) {
        self.last_normalized = None;
        self.consecutive_blanks = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_chinese_subtitle_lines, filter_cjk_text, is_cjk_ideograph, join_sorted_blocks,
        normalized_similarity, sort_text_blocks, FuzzyDeduplicator, TextDeduplicator,
        DEFAULT_BLANK_RESET_FRAMES, DEFAULT_DEDUPE_THRESHOLD,
    };
    use wcocr_rs::TextBlock;

    fn block(left: f32, top: f32, right: f32, bottom: f32, text: &str) -> TextBlock {
        TextBlock {
            left,
            top,
            right,
            bottom,
            rate: 0.99,
            text: text.to_string(),
            details: Vec::new(),
        }
    }

    #[test]
    fn sort_text_blocks_orders_top_to_bottom_and_left_to_right_within_same_line() {
        // Row 1 has slight vertical jitter within half a line height (~10 px on a ~20 px row).
        // Row 2 sits clearly lower around y=140..162.
        let blocks = vec![
            block(320.0, 140.0, 380.0, 162.0, "见"),
            block(260.0, 98.0, 340.0, 118.0, "世界"),
            block(100.0, 100.0, 140.0, 120.0, "你"),
            block(210.0, 142.0, 300.0, 160.0, "明天"),
            block(145.0, 103.0, 200.0, 125.0, "好"),
        ];

        let sorted_texts: Vec<&str> = sort_text_blocks(&blocks)
            .into_iter()
            .map(|b| b.text.as_str())
            .collect();
        assert_eq!(sorted_texts, vec!["你", "好", "世界", "明天", "见"]);
        assert_eq!(join_sorted_blocks(&blocks), "你 好 世界 明天 见");
    }

    #[test]
    fn extract_chinese_subtitle_lines_keeps_two_visual_lines_separate() {
        // Two speakers stacked in one frame stay two utterances.
        let blocks = vec![
            block(100.0, 100.0, 300.0, 120.0, "你好吗？"),
            block(100.0, 130.0, 300.0, 150.0, "我很好。"),
        ];
        assert_eq!(
            extract_chinese_subtitle_lines(&blocks),
            vec!["你好吗？".to_string(), "我很好。".to_string()]
        );
    }

    #[test]
    fn extract_chinese_subtitle_lines_drops_non_cjk_blocks() {
        let blocks = vec![
            block(100.0, 100.0, 300.0, 120.0, "你好"),
            block(100.0, 130.0, 300.0, 150.0, "ABC 123"),
            block(100.0, 160.0, 300.0, 180.0, "再见"),
        ];
        assert_eq!(
            extract_chinese_subtitle_lines(&blocks),
            vec!["你好".to_string(), "再见".to_string()]
        );
        assert!(extract_chinese_subtitle_lines(&[]).is_empty());
    }

    #[test]
    fn filter_cjk_text_keeps_chinese_digits_and_punctuation_while_stripping_latin_noise() {
        assert!(is_cjk_ideograph('夜'));
        assert!(is_cjk_ideograph('\u{3400}'));
        assert!(!is_cjk_ideograph('A'));
        assert!(!is_cjk_ideograph('1'));

        assert_eq!(
            filter_cjk_text("Bilibili 1080P 你好世界 2026"),
            Some("1080 你好世界 2026".to_string())
        );
        assert_eq!(
            filter_cjk_text("第12集：夜深了，雨还在下。"),
            Some("第12集：夜深了，雨还在下。".to_string())
        );
        assert_eq!(
            filter_cjk_text("“测试\u{3400}”……——ＡＢＣ"),
            Some("“测试\u{3400}”……——".to_string())
        );
    }

    #[test]
    fn filter_cjk_text_returns_none_when_zero_cjk_ideographs_exist() {
        assert_eq!(filter_cjk_text(""), None);
        assert_eq!(filter_cjk_text("   \t\n "), None);
        assert_eq!(filter_cjk_text("1080P HD 2026"), None);
        assert_eq!(filter_cjk_text("Netflix Episode 3"), None);
        assert_eq!(filter_cjk_text("，。！？ 12345"), None);
    }

    #[test]
    fn normalized_similarity_handles_exact_partial_and_empty_unicode_strings() {
        assert!((normalized_similarity("夜深了", "夜深了") - 1.0).abs() < 1e-6);
        assert!((normalized_similarity("", "") - 1.0).abs() < 1e-6);
        assert!((normalized_similarity("", "夜深了") - 0.0).abs() < 1e-6);
        assert!((normalized_similarity("夜深了", "") - 0.0).abs() < 1e-6);

        let sim_15 = normalized_similarity(
            "一二三四五六七八九十甲乙丙丁戊",
            "一二三四五六七八九十甲乙丙丁己",
        );
        assert!(
            (sim_15 - (14.0 / 15.0)).abs() < 1e-5,
            "expected 14/15 similarity, got {sim_15}"
        );
        assert!(sim_15 >= DEFAULT_DEDUPE_THRESHOLD);

        let sim_14 = normalized_similarity(
            "一二三四五六七八九十甲乙丙丁",
            "一二三四五六七八九十甲乙丙己",
        );
        assert!(
            (sim_14 - (13.0 / 14.0)).abs() < 1e-5,
            "expected 13/14 similarity, got {sim_14}"
        );
        assert!(sim_14 < DEFAULT_DEDUPE_THRESHOLD);
    }

    #[test]
    fn fuzzy_deduplicator_suppresses_at_or_above_threshold_and_emits_below() {
        let mut deduper = FuzzyDeduplicator::default();
        assert!((deduper.threshold() - 0.93).abs() < 1e-6);
        assert_eq!(deduper.blank_reset_frames(), 5);

        // First 15-char line emits immediately.
        assert_eq!(
            deduper.process(Some("一二三四五六七八九十甲乙丙丁戊")),
            Some("一二三四五六七八九十甲乙丙丁戊".to_string())
        );

        // 1-of-15 substitution => 14/15 = 0.9333 >= 0.93 => suppressed.
        assert_eq!(
            deduper.process(Some("一二三四五六七八九十甲乙丙丁己")),
            None
        );

        // Distinct line (< 0.93) emits immediately.
        assert_eq!(
            deduper.process(Some("今天天气真不错我们出门吧")),
            Some("今天天气真不错我们出门吧".to_string())
        );

        // Punctuation-only flicker against the previous line is normalized out and suppressed.
        assert_eq!(
            deduper.process(Some("夜深了，雨还在下。")),
            Some("夜深了，雨还在下。".to_string())
        );
        assert_eq!(deduper.process(Some("夜深了,雨还在下")), None);
    }

    #[test]
    fn fuzzy_deduplicator_resets_only_after_five_consecutive_blank_frames() {
        let mut deduper =
            FuzzyDeduplicator::new(DEFAULT_DEDUPE_THRESHOLD, DEFAULT_BLANK_RESET_FRAMES);
        assert_eq!(
            deduper.process(Some("夜深了，雨还在下。")),
            Some("夜深了，雨还在下。".to_string())
        );

        // 4 consecutive blank frames are not enough to clear memory.
        for _ in 0..4 {
            assert_eq!(deduper.process(None), None);
        }
        // Duplicate still suppressed, and blank streak resets to 0.
        assert_eq!(deduper.process(Some("夜深了，雨还在下。")), None);

        // 4 more non-CJK/blank frames after the duplicate must still suppress.
        assert_eq!(deduper.process(None), None);
        assert_eq!(deduper.process(Some("...")), None);
        assert_eq!(deduper.process(None), None);
        assert_eq!(deduper.process(Some("   ")), None);
        assert_eq!(deduper.process(Some("夜深了，雨还在下。")), None);

        // 5 consecutive blank/non-CJK frames clear memory so the line re-emits.
        for _ in 0..5 {
            assert_eq!(deduper.process(None), None);
        }
        assert_eq!(
            deduper.process(Some("夜深了，雨还在下。")),
            Some("夜深了，雨还在下。".to_string())
        );

        // Explicit reset clears memory immediately.
        deduper.reset();
        assert_eq!(
            deduper.process(Some("夜深了，雨还在下。")),
            Some("夜深了，雨还在下。".to_string())
        );
    }
}
