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
    group_text_blocks_into_rows(blocks)
        .into_iter()
        .flatten()
        .collect()
}

/// Group OCR `TextBlock`s into visual rows (top-to-bottom), each row sorted
/// left-to-right. Two blocks share a row when their vertical centers fall
/// within half a line height of the row's running average (same rule
/// `sort_text_blocks` has always used — this is that grouping, factored out
/// so the geometric speaker split can work row by row).
pub fn group_text_blocks_into_rows(blocks: &[TextBlock]) -> Vec<Vec<&TextBlock>> {
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

    let mut rows: Vec<Vec<&TextBlock>> = Vec::with_capacity(lines.len());
    for mut line in lines {
        line.blocks.sort_by(|a, b| {
            a.left
                .total_cmp(&b.left)
                .then_with(|| a.top.total_cmp(&b.top))
        });
        rows.push(line.blocks);
    }
    rows
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

/// Horizontal gap, in multiples of the row's median character width, above
/// which two text pieces on the same visual row are different utterances
/// (different speakers), and at or below which they are one sentence the
/// OCR engine happened to return as two boxes.
///
/// Calibrated 2026-11-25 on the real WeChat OCR engine (probe image with a
/// 36 px CJK font at 960x540; see `examples/ocr_gap_probe.rs`): normal
/// inter-character gaps 0-5 px (0-0.16x char width); a same-sentence 12 px
/// gap (0.33x) stayed one block; a 26 px block gap (0.79x) and an 82 px gap
/// (2.3x) came back as separate blocks. 0.5x sits between the largest
/// same-sentence gap measured (0.33x) and the smallest speaker gap (0.79x),
/// with ~1.6x headroom on both sides. CJK glyphs are square, so the
/// median character width is close to the line height; block height is the
/// fallback estimate when no character width can be measured.
pub const SPEAKER_GAP_CHAR_WIDTHS: f32 = 0.5;

/// Whether a geometric decision split two pieces apart or joined them into
/// one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometricSplitAction {
    Split,
    Joined,
}

/// Where a geometric decision was taken: between two blocks/pieces on one
/// row, or between two character boxes inside a single block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometricSplitKind {
    BlockGap,
    CharGap,
}

/// One geometric split/join decision, for the OCR debug log: the gap that
/// was judged, the threshold it was judged against, the boxes on either
/// side of the gap, and the texts that resulted. Recorded so a later run
/// can show why a line was split — or why it was kept whole.
#[derive(Debug, Clone, PartialEq)]
pub struct GeometricSplitDecision {
    pub kind: GeometricSplitKind,
    pub action: GeometricSplitAction,
    pub gap_px: f32,
    pub threshold_px: f32,
    /// Bounding box of the piece left of the gap, as (left, top, right, bottom).
    pub left_box: (f32, f32, f32, f32),
    /// Bounding box of the piece right of the gap, as (left, top, right, bottom).
    pub right_box: (f32, f32, f32, f32),
    pub left_text: String,
    pub right_text: String,
}

/// One piece of a visual row: the text of one block, or of part of a block
/// cut at a large inter-character gap, with the box that text occupies.
#[derive(Debug, Clone)]
struct RowPiece {
    text: String,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
    /// Whether this piece starts at a character-gap cut inside its block.
    /// The `CharGap` decision already explains such a boundary, so the
    /// row-grouping step records no second `BlockGap` decision for it.
    starts_at_char_cut: bool,
}

fn median_of(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[mid])
    } else {
        Some((values[mid - 1] + values[mid]) * 0.5)
    }
}

/// Estimate one block's character width in pixels: the median width of its
/// CJK character boxes when the engine returned them (most accurate), else
/// the block width divided by its CJK character count, else the block height
/// (CJK glyphs are square). Returns `None` when nothing can be measured.
fn block_char_width(block: &TextBlock) -> Option<f32> {
    let mut widths: Vec<f32> = block
        .details
        .iter()
        .filter(|d| d.chars.chars().any(is_cjk_ideograph))
        .map(|d| d.right - d.left)
        .filter(|w| *w > 0.0)
        .collect();
    if let Some(median) = median_of(&mut widths) {
        return Some(median);
    }
    let cjk_count = block.text.chars().filter(|&c| is_cjk_ideograph(c)).count();
    if cjk_count > 0 {
        let width = block.right - block.left;
        if width > 0.0 {
            return Some(width / cjk_count as f32);
        }
    }
    let height = block.bottom - block.top;
    if height > 0.0 {
        return Some(height);
    }
    None
}

/// Median character width of a row's blocks (pixels). `None` when no block
/// in the row yields a measurement — the caller then keeps the old
/// one-line-per-block behavior instead of guessing a threshold.
fn row_median_char_width(blocks: &[&TextBlock]) -> Option<f32> {
    let mut widths: Vec<f32> = blocks.iter().filter_map(|b| block_char_width(b)).collect();
    median_of(&mut widths)
}

/// Whether the per-character boxes cover the block's text: the
/// whitespace-stripped concatenation of the boxes' characters equals the
/// block's whitespace-stripped text. The engine usually returns one box per
/// character (verified on the real engine 2026-11-25, including a space box
/// inside `明天早上 见。`); when it does not, the block is left whole and
/// the character speaker rules downstream decide instead.
fn details_cover_text(block: &TextBlock) -> bool {
    if block.details.is_empty() {
        return false;
    }
    let details_text: String = block.details.iter().flat_map(|d| d.chars.chars()).collect();
    let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    strip(&details_text) == strip(&block.text)
}

/// Split one block at large gaps between its character boxes (a block the
/// engine returned whole can still hold two speakers). Whitespace-only
/// boxes are excluded from the gap measurement (they sit inside the gap
/// itself), but their positions are kept as separators when the piece
/// text is rebuilt, so a sub-threshold space survives for the character
/// speaker rules downstream to judge: geometry decides first at large
/// gaps, characters are the fallback for the rest (merge fix 2,
/// 2026-11-26 — dropping the space used to defeat the whitespace
/// speaker split for single-block reads: raw OCR had the space,
/// `lines[]` did not, and the merged line won the buffer race).
/// Returns the block whole (no decisions) when it has no usable
/// character boxes, and records a `CharGap` decision for every split.
fn split_block_by_char_gaps(
    block: &TextBlock,
    threshold_px: f32,
    decisions: &mut Vec<GeometricSplitDecision>,
) -> Vec<RowPiece> {
    let whole = || {
        vec![RowPiece {
            text: block.text.clone(),
            left: block.left,
            top: block.top,
            right: block.right,
            bottom: block.bottom,
            starts_at_char_cut: false,
        }]
    };
    if !details_cover_text(block) {
        return whole();
    }
    // Character boxes in reading order, whitespace-only boxes excluded
    // from the gap measurement: the gap that matters is CJK-to-CJK, and
    // a space box would split one real gap into two small ones.
    let mut chars: Vec<(String, f32, f32, f32, f32)> = block
        .details
        .iter()
        .filter(|d| !d.chars.chars().all(|c| c.is_whitespace()))
        .map(|d| (d.chars.clone(), d.left, d.top, d.right, d.bottom))
        .collect();
    chars.sort_by(|a, b| a.1.total_cmp(&b.1));
    if chars.len() < 2 {
        return whole();
    }
    // Whether a whitespace box sits between this CJK box and the previous
    // one (by centre x): the space the engine placed between two
    // speakers. Only inter-glyph spaces count; a leading/trailing space
    // is dropped with the trim at the line level.
    let mut spaces: Vec<(f32, f32)> = block
        .details
        .iter()
        .filter(|d| d.chars.chars().all(|c| c.is_whitespace()))
        .map(|d| (d.left, d.right))
        .collect();
    spaces.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut space_before: Vec<bool> = vec![false; chars.len()];
    for idx in 1..chars.len() {
        let prev_right = chars[idx - 1].3;
        let cur_left = chars[idx].1;
        space_before[idx] = spaces.iter().any(|&(s_left, s_right)| {
            let centre = (s_left + s_right) * 0.5;
            centre > prev_right - 1.0 && centre < cur_left + 1.0
        });
    }
    // Cut points: indices where the gap to the previous character exceeds
    // the threshold. Character boxes can overlap slightly (measured on the
    // real engine), so a negative gap means "touching", never a split.
    let cuts: Vec<usize> = (1..chars.len())
        .filter(|&idx| (chars[idx].1 - chars[idx - 1].3).max(0.0) > threshold_px)
        .collect();
    let piece_text = |start: usize, end: usize| {
        let mut out = String::new();
        for idx in start..end {
            if idx > start && space_before[idx] {
                out.push(' ');
            }
            out.push_str(&chars[idx].0);
        }
        out
    };
    // No cut: rebuild the text from the character boxes, keeping a
    // sub-threshold space where the engine placed one — the character
    // speaker rules downstream judge it (their guardrails block splits
    // inside numbers, dates, and one-character interjections), and
    // `split_speakers` strips any space that does not split, so no stray
    // space reaches the log or the translation.
    if cuts.is_empty() {
        let last = &chars[chars.len() - 1];
        return vec![RowPiece {
            text: piece_text(0, chars.len()),
            left: chars[0].1,
            top: chars[0].2,
            right: last.3,
            bottom: last.4,
            starts_at_char_cut: false,
        }];
    }
    let mut pieces: Vec<RowPiece> = Vec::new();
    let mut start = 0usize;
    for &cut in cuts.iter().chain(std::iter::once(&chars.len())) {
        let last = &chars[cut - 1];
        pieces.push(RowPiece {
            text: piece_text(start, cut),
            left: chars[start].1,
            top: chars[start].2,
            right: last.3,
            bottom: last.4,
            starts_at_char_cut: start != 0,
        });
        start = cut;
    }
    for (idx, &cut) in cuts.iter().enumerate() {
        let prev = &chars[cut - 1];
        let next = &chars[cut];
        decisions.push(GeometricSplitDecision {
            kind: GeometricSplitKind::CharGap,
            action: GeometricSplitAction::Split,
            gap_px: (next.1 - prev.3).max(0.0),
            threshold_px,
            left_box: (prev.1, prev.2, prev.3, prev.4),
            right_box: (next.1, next.2, next.3, next.4),
            left_text: pieces[idx].text.clone(),
            right_text: pieces[idx + 1].text.clone(),
        });
    }
    pieces
}

/// Sort `blocks` in reading order and filter each visual line to Chinese
/// subtitle text, returning one non-empty Chinese line per utterance
/// (top-to-bottom). Two visual lines in one OCR frame stay separate here
/// so the pipeline can treat them as two utterances (A2 / gh #1) instead
/// of one merged line.
///
/// Geometric speaker split (merge fix, 2026-11-25): within one visual row,
/// blocks whose horizontal gap exceeds `SPEAKER_GAP_CHAR_WIDTHS` x the
/// row's median character width stay separate lines, while blocks that
/// nearly abut are joined into one line (one sentence the engine happened
/// to return as two boxes). A single block is further split at large gaps
/// between its character boxes. Priority is geometry first at large gaps;
/// a sub-threshold space the engine placed inside a block is preserved in
/// the line so the character speaker rules in `pipeline` can judge it
/// (merge fix 2, 2026-11-26); blocks without usable character boxes keep
/// their text as-is and the character rules decide instead.
/// Returns an empty `Vec` when no block contains CJK text.
pub fn extract_chinese_subtitle_lines(blocks: &[TextBlock]) -> Vec<String> {
    extract_chinese_subtitle_lines_with_decisions(blocks).0
}

/// Like `extract_chinese_subtitle_lines`, but also returns the geometric
/// split/join decisions, for the OCR debug log.
pub fn extract_chinese_subtitle_lines_with_decisions(
    blocks: &[TextBlock],
) -> (Vec<String>, Vec<GeometricSplitDecision>) {
    let mut decisions = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for row in group_text_blocks_into_rows(blocks) {
        let threshold_px = match row_median_char_width(&row) {
            Some(median) => SPEAKER_GAP_CHAR_WIDTHS * median,
            // No measurable character width in this row: fall back to the
            // old one-line-per-block behavior rather than guessing.
            None => {
                for block in row {
                    if let Some(line) = filter_cjk_text(block.text.trim()) {
                        lines.push(line);
                    }
                }
                continue;
            }
        };
        // Split each block internally at large character gaps, then group
        // the resulting pieces into lines by their horizontal gaps.
        let mut pieces: Vec<RowPiece> = Vec::new();
        for block in row {
            pieces.extend(split_block_by_char_gaps(
                block,
                threshold_px,
                &mut decisions,
            ));
        }
        let mut groups: Vec<Vec<RowPiece>> = Vec::new();
        let mut current: Vec<RowPiece> = Vec::new();
        for piece in pieces {
            if let Some(prev) = current.last() {
                let gap = (piece.left - prev.right).max(0.0);
                if gap > threshold_px {
                    // A boundary at a character-gap cut is already covered
                    // by its `CharGap` decision; do not log it twice.
                    if !piece.starts_at_char_cut {
                        decisions.push(GeometricSplitDecision {
                            kind: GeometricSplitKind::BlockGap,
                            action: GeometricSplitAction::Split,
                            gap_px: gap,
                            threshold_px,
                            left_box: (prev.left, prev.top, prev.right, prev.bottom),
                            right_box: (piece.left, piece.top, piece.right, piece.bottom),
                            left_text: current
                                .iter()
                                .map(|p| p.text.trim())
                                .collect::<Vec<_>>()
                                .concat(),
                            right_text: piece.text.trim().to_string(),
                        });
                    }
                    groups.push(std::mem::take(&mut current));
                } else if !piece.starts_at_char_cut {
                    decisions.push(GeometricSplitDecision {
                        kind: GeometricSplitKind::BlockGap,
                        action: GeometricSplitAction::Joined,
                        gap_px: gap,
                        threshold_px,
                        left_box: (prev.left, prev.top, prev.right, prev.bottom),
                        right_box: (piece.left, piece.top, piece.right, piece.bottom),
                        left_text: current
                            .iter()
                            .map(|p| p.text.trim())
                            .collect::<Vec<_>>()
                            .concat(),
                        right_text: piece.text.trim().to_string(),
                    });
                }
            }
            current.push(piece);
        }
        if !current.is_empty() {
            groups.push(current);
        }
        for group in groups {
            let joined = group
                .iter()
                .map(|p| p.text.trim())
                .collect::<Vec<_>>()
                .concat();
            if let Some(line) = filter_cjk_text(&joined) {
                lines.push(line);
            }
        }
    }
    (lines, decisions)
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
    /// Similarity of the most recent comparison against the previous
    /// subtitle (`None` when nothing was compared yet, or after the
    /// memory was cleared). Read by the OCR debug log so a dropped line
    /// can show how close it was to the previous one (merge fix, 2026-11-25).
    last_similarity: Option<f32>,
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
            last_similarity: None,
        }
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn blank_reset_frames(&self) -> u32 {
        self.blank_reset_frames
    }

    /// Similarity of the most recent comparison against the previous
    /// subtitle, if one was compared (merge fix, 2026-11-25).
    pub fn last_similarity(&self) -> Option<f32> {
        self.last_similarity
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
                self.last_similarity = None;
            }
            return None;
        };

        self.consecutive_blanks = 0;
        // Record the comparison even when it passes: the debug log reads
        // it for the lines the dedupe drops.
        let prev_similarity = self
            .last_normalized
            .as_ref()
            .map(|prev| normalized_similarity(&norm, prev));
        self.last_similarity = prev_similarity;
        if let Some(similarity) = prev_similarity {
            if similarity >= self.threshold {
                return None;
            }
        }

        self.last_normalized = Some(norm);
        filtered_text.map(str::to_owned)
    }

    fn reset(&mut self) {
        self.last_normalized = None;
        self.consecutive_blanks = 0;
        self.last_similarity = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        extract_chinese_subtitle_lines, extract_chinese_subtitle_lines_with_decisions,
        filter_cjk_text, is_cjk_ideograph, join_sorted_blocks, normalized_similarity,
        sort_text_blocks, FuzzyDeduplicator, GeometricSplitAction, GeometricSplitKind,
        TextDeduplicator, DEFAULT_BLANK_RESET_FRAMES, DEFAULT_DEDUPE_THRESHOLD,
    };
    use wcocr_rs::{CharDetail, TextBlock};

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

    /// A block with per-character boxes: `details` is a list of
    /// (characters, left, top, right, bottom), in reading order.
    fn block_with_details(
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
        text: &str,
        details: &[(&str, f32, f32, f32, f32)],
    ) -> TextBlock {
        TextBlock {
            left,
            top,
            right,
            bottom,
            rate: 0.99,
            text: text.to_string(),
            details: details
                .iter()
                .map(|(chars, l, t, r, b)| CharDetail {
                    chars: (*chars).to_string(),
                    left: *l,
                    top: *t,
                    right: *r,
                    bottom: *b,
                })
                .collect(),
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
    fn extract_lines_splits_same_row_blocks_at_a_large_gap() {
        // Two blocks on one row, 80 px apart (median char width 40 px, so
        // the threshold is 20 px): two speakers, two lines. Mirrors the
        // probe image row 1 (gap 82 px measured on the real engine).
        let blocks = vec![
            block(100.0, 100.0, 220.0, 140.0, "你好吗"),
            block(300.0, 100.0, 420.0, 140.0, "我很好"),
        ];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["你好吗".to_string(), "我很好".to_string()]);
        assert_eq!(decisions.len(), 1);
        let decision = &decisions[0];
        assert_eq!(decision.kind, GeometricSplitKind::BlockGap);
        assert_eq!(decision.action, GeometricSplitAction::Split);
        assert!(
            (decision.gap_px - 80.0).abs() < 1e-6,
            "gap {}",
            decision.gap_px
        );
        assert!(
            (decision.threshold_px - 20.0).abs() < 1e-6,
            "threshold {}",
            decision.threshold_px
        );
        assert_eq!(decision.left_text, "你好吗");
        assert_eq!(decision.right_text, "我很好");
    }

    #[test]
    fn extract_lines_joins_same_row_blocks_at_a_character_sized_gap() {
        // Two blocks on one row, 12 px apart (median char width 36 px, so
        // the threshold is 18 px): one sentence the engine returned as two
        // boxes — joined into one line, no space for the character speaker
        // rules downstream to re-split on.
        let blocks = vec![
            block(100.0, 100.0, 208.0, 140.0, "你好吗"),
            block(220.0, 100.0, 328.0, 140.0, "我很好"),
        ];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["你好吗我很好".to_string()]);
        assert_eq!(decisions.len(), 1);
        let decision = &decisions[0];
        assert_eq!(decision.kind, GeometricSplitKind::BlockGap);
        assert_eq!(decision.action, GeometricSplitAction::Joined);
        assert!(
            (decision.gap_px - 12.0).abs() < 1e-6,
            "gap {}",
            decision.gap_px
        );
    }

    #[test]
    fn extract_lines_splits_one_block_at_a_large_character_gap() {
        // One block whose character boxes hold two groups 91 px apart
        // (median char width 38 px, threshold 19 px): two speakers inside
        // a single block. Only the CharGap decision is recorded — the
        // row-grouping step does not log the same boundary a second time.
        let blocks = vec![block_with_details(
            95.0,
            85.0,
            410.0,
            129.0,
            "你好吗我很好",
            &[
                ("你", 95.0, 85.0, 127.0, 127.0),
                ("好", 127.0, 85.0, 165.0, 127.0),
                ("吗", 165.0, 85.0, 203.0, 127.0),
                ("我", 294.0, 86.0, 332.0, 128.0),
                ("很", 326.0, 86.0, 364.0, 128.0),
                ("好", 364.0, 86.0, 402.0, 128.0),
            ],
        )];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["你好吗".to_string(), "我很好".to_string()]);
        assert_eq!(decisions.len(), 1);
        let decision = &decisions[0];
        assert_eq!(decision.kind, GeometricSplitKind::CharGap);
        assert_eq!(decision.action, GeometricSplitAction::Split);
        assert!(
            (decision.gap_px - 91.0).abs() < 1e-6,
            "gap {}",
            decision.gap_px
        );
        assert!(
            (decision.threshold_px - 19.0).abs() < 1e-6,
            "threshold {}",
            decision.threshold_px
        );
        assert_eq!(decision.left_text, "你好吗");
        assert_eq!(decision.right_text, "我很好");
    }

    #[test]
    fn extract_lines_keeps_one_block_whole_at_a_small_character_gap() {
        // One block, `明天早上 见。` with a 10 px gap between 上 and 见
        // (median CJK char width 37 px, threshold 18.5 px): one sentence —
        // kept whole as one line, and the sub-threshold space is now
        // PRESERVED in the line (merge fix 2) so the character speaker
        // rules downstream can judge it; they keep it whole (the right
        // cluster 見 is one character, below the 2-character guardrail)
        // and strip the space in the final piece. Character boxes and
        // text taken from the real engine probe (2026-11-25).
        let blocks = vec![block_with_details(
            96.0,
            186.0,
            306.0,
            228.0,
            "明天早上 见。",
            &[
                ("明", 96.0, 186.0, 128.0, 227.0),
                ("天", 128.0, 186.0, 164.0, 227.0),
                ("早", 164.0, 186.0, 201.0, 227.0),
                ("上", 201.0, 186.0, 238.0, 227.0),
                (" ", 238.0, 186.0, 254.0, 227.0),
                ("见", 248.0, 186.0, 285.0, 227.0),
                ("。", 280.0, 186.0, 306.0, 227.0),
            ],
        )];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["明天早上 见。".to_string()]);
        assert!(decisions.is_empty(), "unexpected decisions: {decisions:?}");
    }

    #[test]
    fn extract_lines_preserves_space_inside_one_block_for_speaker_rules() {
        // Merge fix 2, real video 2026-11-26 segment #4: one block
        // `走了哦 走好爸爸` whose CJK-to-CJK gap at the space (17 px)
        // is below the 18 px threshold (median char width 36 px), so
        // geometry does not split — but the space itself must survive in
        // the line for the character speaker rules downstream, which
        // split it into `走了哦` / `走好爸爸`. Previously the line came
        // out spaceless and the merged line won the buffer race.
        let blocks = vec![block_with_details(
            141.0,
            42.0,
            415.0,
            88.0,
            "走了哦 走好爸爸",
            &[
                ("走", 141.0, 42.0, 177.0, 88.0),
                ("了", 178.0, 42.0, 214.0, 88.0),
                ("哦", 215.0, 42.0, 251.0, 88.0),
                (" ", 252.0, 42.0, 267.0, 88.0),
                ("走", 268.0, 42.0, 304.0, 88.0),
                ("好", 305.0, 42.0, 341.0, 88.0),
                ("爸", 342.0, 42.0, 378.0, 88.0),
                ("爸", 379.0, 42.0, 415.0, 88.0),
            ],
        )];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["走了哦 走好爸爸".to_string()]);
        assert!(decisions.is_empty(), "unexpected decisions: {decisions:?}");
    }

    #[test]
    fn extract_lines_preserves_two_spaces_inside_one_block() {
        // Merge fix 2, real video 2026-11-26 segment #21: one block
        // `好吧 可能錢太多了 對吧` with two sub-threshold spaces
        // (16 px each, threshold 18 px): both survive in the line, and
        // the speaker rules split it into three utterances.
        let blocks = vec![block_with_details(
            100.0,
            42.0,
            499.0,
            88.0,
            "好吧 可能錢太多了 對吧",
            &[
                ("好", 100.0, 42.0, 136.0, 88.0),
                ("吧", 137.0, 42.0, 173.0, 88.0),
                (" ", 174.0, 42.0, 188.0, 88.0),
                ("可", 189.0, 42.0, 225.0, 88.0),
                ("能", 226.0, 42.0, 262.0, 88.0),
                ("錢", 263.0, 42.0, 299.0, 88.0),
                ("太", 300.0, 42.0, 336.0, 88.0),
                ("多", 337.0, 42.0, 373.0, 88.0),
                ("了", 374.0, 42.0, 410.0, 88.0),
                (" ", 411.0, 42.0, 425.0, 88.0),
                ("對", 426.0, 42.0, 462.0, 88.0),
                ("吧", 463.0, 42.0, 499.0, 88.0),
            ],
        )];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["好吧 可能錢太多了 對吧".to_string()]);
        assert!(decisions.is_empty(), "unexpected decisions: {decisions:?}");
    }

    #[test]
    fn extract_lines_space_at_a_large_gap_cut_leaks_into_neither_piece() {
        // One block with a space box sitting inside a 91 px character
        // gap (threshold 19 px): geometry splits at the gap, and the
        // space — which IS the gap — belongs to neither piece.
        let blocks = vec![block_with_details(
            95.0,
            85.0,
            410.0,
            129.0,
            "你好吗我很好",
            &[
                ("你", 95.0, 85.0, 127.0, 127.0),
                ("好", 127.0, 85.0, 165.0, 127.0),
                ("吗", 165.0, 85.0, 203.0, 127.0),
                (" ", 204.0, 85.0, 250.0, 127.0),
                ("我", 294.0, 86.0, 332.0, 128.0),
                ("很", 326.0, 86.0, 364.0, 128.0),
                ("好", 364.0, 86.0, 402.0, 128.0),
            ],
        )];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["你好吗".to_string(), "我很好".to_string()]);
        assert_eq!(decisions.len(), 1);
        let decision = &decisions[0];
        assert_eq!(decision.kind, GeometricSplitKind::CharGap);
        assert_eq!(decision.action, GeometricSplitAction::Split);
        assert!(
            (decision.gap_px - 91.0).abs() < 1e-6,
            "gap {}",
            decision.gap_px
        );
        assert_eq!(decision.left_text, "你好吗");
        assert_eq!(decision.right_text, "我很好");
    }

    #[test]
    fn extract_lines_falls_back_to_character_rules_without_details() {
        // A block with no character boxes keeps its text as-is (space
        // included): geometry cannot judge it, so the character speaker
        // rules in the pipeline decide instead. No decisions recorded.
        let blocks = vec![block(100.0, 100.0, 300.0, 140.0, "你好吗 我很好")];
        let (lines, decisions) = extract_chinese_subtitle_lines_with_decisions(&blocks);
        assert_eq!(lines, vec!["你好吗 我很好".to_string()]);
        assert!(decisions.is_empty(), "unexpected decisions: {decisions:?}");
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
    fn fuzzy_deduplicator_records_last_similarity_for_the_debug_log() {
        let mut deduper = FuzzyDeduplicator::default();
        assert_eq!(deduper.last_similarity(), None);
        assert_eq!(
            deduper.process(Some("一二三四五六七八九十甲乙丙丁戊")),
            Some("一二三四五六七八九十甲乙丙丁戊".to_string())
        );
        // First line: nothing to compare against.
        assert_eq!(deduper.last_similarity(), None);
        // 1-of-15 substitution => 14/15 = 0.9333, suppressed.
        assert_eq!(
            deduper.process(Some("一二三四五六七八九十甲乙丙丁己")),
            None
        );
        let sim = deduper.last_similarity().unwrap();
        assert!((sim - 14.0 / 15.0).abs() < 1e-4, "got {sim}");
        // A distinct line still records its (low) similarity.
        assert_eq!(
            deduper.process(Some("今天天气真不错我们出门吧")),
            Some("今天天气真不错我们出门吧".to_string())
        );
        assert!(deduper.last_similarity().unwrap() < 0.5);
        // Clearing the memory clears the similarity too.
        deduper.reset();
        assert_eq!(deduper.last_similarity(), None);
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
