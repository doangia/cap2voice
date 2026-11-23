//! QuickTranslate-compatible rule engine ported from `vietphrase.user.js`
//! (via `server/vietphrase/rule_engine.py`).
//!
//! Only the pure, DOM-free part is ported: rule parsing, number
//! conversion, dictionary lookups and `match_at`. All semantics (capture
//! sources, specificity ordering, number boundary guards, VP-overwrite
//! protection) mirror the JavaScript line by line so the two
//! implementations produce identical translations.
//!
//! The engine works on `&[char]` slices (char indices), mirroring Python's
//! `str` indexing by code point. Callers convert the source text to chars
//! once and reuse the slice.

use std::collections::{HashMap, HashSet};

/// Chinese numeral digits (traditional + simplified + ASCII handled separately).
fn num_digits(ch: char) -> Option<u32> {
    match ch {
        '零' | '〇' | '○' => Some(0),
        '一' => Some(1),
        '二' | '两' | '兩' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

fn small_unit(ch: char) -> Option<u32> {
    match ch {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1000),
        _ => None,
    }
}

fn large_unit(ch: char) -> Option<u32> {
    match ch {
        '万' | '萬' => Some(10000),
        '亿' | '億' => Some(100000000),
        _ => None,
    }
}

/// Characters that may appear in a Chinese numeral (Python `NUMERIC_CHARS`).
fn is_numeric_char(ch: char) -> bool {
    ch.is_ascii_digit()
        || num_digits(ch).is_some()
        || small_unit(ch).is_some()
        || large_unit(ch).is_some()
}

/// Characters that may appear in a year (Python `YEAR_RE`).
fn is_year_char(ch: char) -> bool {
    ch.is_ascii_digit() || num_digits(ch).is_some()
}

/// Chapter/volume-style labels (Python `LABELS`).
fn label_for(ch: char) -> Option<&'static str> {
    match ch {
        '章' => Some("Chương"),
        '卷' => Some("Quyển"),
        '集' => Some("Tập"),
        '节' | '節' => Some("Tiết"),
        '幕' => Some("Màn"),
        '回' => Some("Hồi"),
        '折' => Some("Chiết"),
        _ => None,
    }
}

/// Single-character anchors considered "common" (sorted last as anchors).
const COMMON_ANCHORS: [char; 5] = ['的', '了', '是', '不', '在'];

/// Valid capture sources (Python `CAPTURE_SOURCES`).
const CAPTURE_SOURCES: [&str; 8] = ["n", "y", "L", "ne", "pn", "vp", "hv", "w"];

/// Return the first sense of a dictionary value (`/` or `¦` split).
pub fn first_meaning(value: &str, separator: &str) -> String {
    let mut end = value.len();
    for sep in [separator, "¦"] {
        if sep.is_empty() {
            continue;
        }
        if let Some(at) = value.find(sep) {
            if at < end {
                end = at;
            }
        }
    }
    value[..end].trim().to_string()
}

fn digit_of(ch: char) -> Option<u32> {
    if ch.is_ascii_digit() {
        return ch.to_digit(10);
    }
    num_digits(ch)
}

/// Convert a year written with Chinese digits into Latin digits.
pub fn year_to_latin(raw: &str) -> Option<String> {
    let mut out = String::new();
    for ch in raw.chars() {
        out.push(char::from_digit(digit_of(ch)?, 10)?);
    }
    Some(out)
}

/// Convert a Chinese numeral / year string into Latin digits.
pub fn chinese_to_latin(raw: &str) -> Option<String> {
    if !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit()) {
        // `str(int(raw))` drops leading zeros.
        let n: u64 = raw.parse().ok()?;
        return Some(n.to_string());
    }
    if !raw
        .chars()
        .any(|c| small_unit(c).is_some() || large_unit(c).is_some())
    {
        return year_to_latin(raw);
    }

    let mut total: u64 = 0;
    let mut section: u64 = 0;
    let mut number: u64 = 0;
    let mut saw = false;
    for ch in raw.chars() {
        if let Some(digit) = digit_of(ch) {
            number = u64::from(digit);
            saw = true;
            continue;
        }
        if let Some(small) = small_unit(ch) {
            section += (if number == 0 { 1 } else { number }) * u64::from(small);
            number = 0;
            saw = true;
            continue;
        }
        if let Some(large) = large_unit(ch) {
            section += number;
            total += (if section == 0 { 1 } else { section }) * u64::from(large);
            section = 0;
            number = 0;
            saw = true;
            continue;
        }
        return None;
    }
    if saw {
        Some((total + section + number).to_string())
    } else {
        None
    }
}

/// A rule line failed validation; carries the JS error message verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleParseError(pub String);

impl std::fmt::Display for RuleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RuleParseError {}

/// One token of a compiled rule pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Capture {
        sources: Vec<String>,
        min: usize,
        max: usize,
    },
    Literal {
        value: Vec<char>,
    },
    Group {
        alternatives: Vec<Vec<char>>,
        optional: bool,
    },
}

/// The literal/group anchor a rule is indexed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub value: Vec<char>,
    pub token_index: usize,
}

/// A compiled rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub pattern: String,
    pub translation: String,
    pub line: usize,
    pub tokens: Vec<Token>,
    pub anchor: Anchor,
    pub specificity: usize,
    pub wildcard_count: usize,
    pub order: usize,
}

/// A rule line that failed to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    pub line: usize,
    pub reason: String,
    pub source: String,
}

/// Result of parsing a `rule.txt` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseRulesResult {
    pub rules: Vec<Rule>,
    pub errors: Vec<RuleError>,
}

/// The four dictionaries the rule engine consults.
#[derive(Debug, Clone, Default)]
pub struct EngineDicts {
    pub pa: HashMap<String, String>,
    pub vp: HashMap<String, String>,
    pub names: HashMap<String, String>,
    pub pronouns: HashMap<String, String>,
}

/// One capture produced while matching a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureMatch {
    pub len: usize,
    pub raw: String,
    pub value: String,
}

/// A successful `match_at` hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchHit {
    pub start: usize,
    pub end: usize,
    pub source: String,
    pub translation: String,
    pub pattern: String,
    pub line: usize,
}

fn parse_range(raw: &str, default_max: usize) -> Result<(usize, usize), RuleParseError> {
    if raw.is_empty() {
        return Ok((1, default_max));
    }
    // `(\d+)(?:-(\d+))?` on the whole string.
    let (min_s, max_s) = match raw.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (raw, None),
    };
    if min_s.is_empty()
        || !min_s.bytes().all(|b| b.is_ascii_digit())
        || max_s.is_some_and(|m| m.is_empty() || !m.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(RuleParseError("range không hợp lệ".to_string()));
    }
    let minimum: usize = min_s
        .parse()
        .map_err(|_| RuleParseError("range không hợp lệ".to_string()))?;
    let maximum: usize = match max_s {
        Some(m) => m
            .parse()
            .map_err(|_| RuleParseError("range không hợp lệ".to_string()))?,
        None => minimum,
    };
    if minimum < 1 || maximum < minimum || maximum > 64 {
        return Err(RuleParseError("range không hợp lệ".to_string()));
    }
    Ok((minimum, maximum))
}

fn parse_capture(raw: &str) -> Result<Token, RuleParseError> {
    let parts: Vec<&str> = raw.split(':').collect();
    if parts.len() > 2 {
        return Err(RuleParseError("token không hợp lệ".to_string()));
    }
    let sources: Vec<String> = parts[0].split('|').map(str::to_string).collect();
    if sources.is_empty()
        || sources
            .iter()
            .any(|s| !CAPTURE_SOURCES.contains(&s.as_str()))
    {
        return Err(RuleParseError(format!("token lạ: <{raw}>")));
    }
    let fixed = sources.len() == 1 && (sources[0] == "L" || sources[0] == "hv");
    let (minimum, maximum) = parse_range(
        if parts.len() > 1 { parts[1] } else { "" },
        if fixed { 1 } else { 12 },
    )?;
    if fixed && (minimum != 1 || maximum != 1) {
        return Err(RuleParseError(format!(
            "token <{}> chỉ nhận 1 ký tự",
            sources[0]
        )));
    }
    Ok(Token::Capture {
        sources,
        min: minimum,
        max: maximum,
    })
}

fn parse_group(pattern: &[char], start: usize) -> Result<(Token, usize), RuleParseError> {
    let mut end = start + 1;
    let mut escaped = false;
    while end < pattern.len() {
        let ch = pattern[end];
        if escaped {
            escaped = false;
            end += 1;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            end += 1;
            continue;
        }
        if ch == ')' {
            break;
        }
        end += 1;
    }
    if end >= pattern.len() {
        return Err(RuleParseError("thiếu dấu )".to_string()));
    }
    let body: String = pattern[start + 1..end].iter().collect();
    // Faithful to the Python port: split on every `|` first, then unescape
    // `\|` / `\(` / `\)` inside each alternative.
    let alternatives: Vec<Vec<char>> = body
        .split('|')
        .map(|alt| {
            let mut out = Vec::new();
            let mut chars = alt.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        if matches!(next, '|' | '(' | ')') {
                            out.push(next);
                        } else {
                            out.push(ch);
                            out.push(next);
                        }
                    } else {
                        out.push(ch);
                    }
                } else {
                    out.push(ch);
                }
            }
            out
        })
        .collect();
    if alternatives.iter().any(|alt| alt.is_empty()) {
        return Err(RuleParseError("nhóm rỗng".to_string()));
    }
    let optional = pattern.get(end + 1) == Some(&'?');
    Ok((
        Token::Group {
            alternatives,
            optional,
        },
        end + if optional { 2 } else { 1 },
    ))
}

fn parse_pattern(pattern: &str) -> Result<Vec<Token>, RuleParseError> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut literal: Vec<char> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '<' {
            if !literal.is_empty() {
                tokens.push(Token::Literal {
                    value: std::mem::take(&mut literal),
                });
            }
            // Char index of the closing `>` (not a byte index: the
            // capture body may contain multi-byte chars).
            let Some(end) = chars[i + 1..].iter().position(|c| *c == '>') else {
                return Err(RuleParseError("thiếu dấu >".to_string()));
            };
            let inner: String = chars[i + 1..i + 1 + end].iter().collect();
            tokens.push(parse_capture(&inner)?);
            i = i + 1 + end + 1;
        } else if ch == '(' {
            if !literal.is_empty() {
                tokens.push(Token::Literal {
                    value: std::mem::take(&mut literal),
                });
            }
            let (token, next) = parse_group(&chars, i)?;
            tokens.push(token);
            i = next;
        } else {
            literal.push(ch);
            i += 1;
        }
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal { value: literal });
    }
    Ok(tokens)
}

fn get_anchor(tokens: &[Token]) -> Option<Anchor> {
    let mut candidates: Vec<Anchor> = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::Literal { value } if !value.is_empty() => {
                candidates.push(Anchor {
                    value: value.clone(),
                    token_index: i,
                });
            }
            Token::Group {
                alternatives,
                optional,
            } if !optional => {
                for value in alternatives {
                    candidates.push(Anchor {
                        value: value.clone(),
                        token_index: i,
                    });
                }
            }
            _ => {}
        }
    }
    // Sort by (common, -len): non-common anchors first, longer first.
    // Stable sort keeps insertion order on ties, like Python's `list.sort`.
    candidates.sort_by(|a, b| {
        let common_a = u8::from(a.value.len() == 1 && COMMON_ANCHORS.contains(&a.value[0]));
        let common_b = u8::from(b.value.len() == 1 && COMMON_ANCHORS.contains(&b.value[0]));
        common_a
            .cmp(&common_b)
            .then_with(|| b.value.len().cmp(&a.value.len()))
    });
    candidates.into_iter().next()
}

fn compile_rule(pattern: &str, translation: &str, line: usize) -> Result<Rule, RuleParseError> {
    let tokens = parse_pattern(pattern)?;
    let captures = tokens
        .iter()
        .filter(|t| matches!(t, Token::Capture { .. }))
        .count();
    let Some(anchor) = get_anchor(&tokens) else {
        return Err(RuleParseError("rule không có neo".to_string()));
    };
    if captures == 0 {
        return Err(RuleParseError("rule không có wildcard".to_string()));
    }

    // `{N}` placeholders must exist and cover every capture exactly once.
    let mut used: HashSet<usize> = HashSet::new();
    {
        let mut rest = translation;
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                // No closing brace: skip this `{` and keep scanning,
                // like Python's `re.findall`.
                rest = after;
                continue;
            };
            let num = &after[..close];
            if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(n) = num.parse::<usize>() {
                    used.insert(n);
                }
            }
            rest = &after[close + 1..];
        }
    }
    if used.iter().any(|n| *n >= captures) {
        return Err(RuleParseError("placeholder không tồn tại".to_string()));
    }
    if (0..captures).any(|i| !used.contains(&i)) {
        return Err(RuleParseError("capture không được dùng".to_string()));
    }

    let mut specificity = 0;
    for token in &tokens {
        specificity += match token {
            Token::Literal { value } => value.len(),
            Token::Group { alternatives, .. } => {
                alternatives.iter().map(Vec::len).max().unwrap_or(0)
            }
            Token::Capture { max, .. } => *max,
        };
    }

    Ok(Rule {
        pattern: pattern.to_string(),
        translation: translation.to_string(),
        line,
        tokens,
        anchor,
        specificity,
        wildcard_count: captures,
        order: 0,
    })
}

/// Parse a `rule.txt` file into sorted rules + per-line errors.
pub fn parse_rules(text: &str) -> ParseRulesResult {
    let mut rules: Vec<Rule> = Vec::new();
    let mut errors: Vec<RuleError> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (index, raw) in split_lines(text).iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        let Some(eq) = line.find('=') else {
            errors.push(RuleError {
                line: index + 1,
                reason: "thiếu dấu =".to_string(),
                source: (*raw).to_string(),
            });
            continue;
        };
        if eq < 1 {
            errors.push(RuleError {
                line: index + 1,
                reason: "thiếu dấu =".to_string(),
                source: (*raw).to_string(),
            });
            continue;
        }
        let pattern = line[..eq].trim();
        let translation = line[eq + 1..].trim();
        if translation.is_empty() {
            errors.push(RuleError {
                line: index + 1,
                reason: "bản dịch rỗng".to_string(),
                source: (*raw).to_string(),
            });
            continue;
        }
        if seen.contains(pattern) {
            errors.push(RuleError {
                line: index + 1,
                reason: "duplicate".to_string(),
                source: (*raw).to_string(),
            });
            continue;
        }
        match compile_rule(pattern, translation, index + 1) {
            Ok(rule) => {
                seen.insert(pattern.to_string());
                rules.push(rule);
            }
            Err(err) => {
                errors.push(RuleError {
                    line: index + 1,
                    reason: err.0,
                    source: (*raw).to_string(),
                });
            }
        }
    }

    rules.sort_by(|a, b| {
        b.specificity
            .cmp(&a.specificity)
            .then_with(|| a.wildcard_count.cmp(&b.wildcard_count))
            .then_with(|| a.pattern.cmp(&b.pattern))
    });
    ParseRulesResult { rules, errors }
}

/// Split on `\r?\n` (Python `re.split(r"\r?\n", text)`).
pub fn split_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect()
}

fn lookup_source(
    source: &str,
    raw: &str,
    dictionaries: &EngineDicts,
    separator: &str,
) -> Option<String> {
    match source {
        "n" => {
            if raw.is_empty() || !raw.chars().all(is_numeric_char) {
                return None;
            }
            chinese_to_latin(raw)
        }
        "y" => {
            if raw.is_empty() || !raw.chars().all(is_year_char) {
                return None;
            }
            year_to_latin(raw)
        }
        "L" => {
            if raw.chars().count() != 1 {
                return None;
            }
            label_for(raw.chars().next()?).map(str::to_string)
        }
        "hv" => {
            if raw.chars().count() != 1 {
                return None;
            }
            dictionaries
                .pa
                .get(raw)
                .map(|v| first_meaning(v, separator))
        }
        "ne" | "pn" | "vp" | "w" => {
            let order: &[&HashMap<String, String>] = match source {
                "ne" => &[&dictionaries.names],
                "pn" => &[&dictionaries.pronouns],
                "vp" => &[&dictionaries.vp],
                _ => &[
                    &dictionaries.names,
                    &dictionaries.pronouns,
                    &dictionaries.vp,
                ],
            };
            for dict in order {
                if let Some(value) = dict.get(raw) {
                    return Some(first_meaning(value, separator));
                }
            }
            None
        }
        _ => None,
    }
}

fn capture_matches(
    token: &Token,
    text: &[char],
    position: usize,
    dictionaries: &EngineDicts,
    separator: &str,
) -> Vec<CaptureMatch> {
    let Token::Capture { sources, min, max } = token else {
        return Vec::new();
    };
    let available = (*max).min(text.len().saturating_sub(position));
    let mut matches: Vec<CaptureMatch> = Vec::new();
    for source in sources {
        let mut len = available;
        while len >= *min {
            let raw: String = text[position..position + len].iter().collect();
            if let Some(value) = lookup_source(source, &raw, dictionaries, separator) {
                matches.push(CaptureMatch { len, raw, value });
            }
            if len == 0 {
                break;
            }
            len -= 1;
        }
        if !matches.is_empty() {
            break;
        }
    }
    matches
}

struct WalkHit {
    end: usize,
    captures: Vec<CaptureMatch>,
}

#[allow(clippy::too_many_arguments)]
fn walk_rule(
    rule: &Rule,
    text: &[char],
    token_index: usize,
    position: usize,
    captures: Vec<CaptureMatch>,
    dictionaries: &EngineDicts,
    separator: &str,
) -> Option<WalkHit> {
    if token_index == rule.tokens.len() {
        return Some(WalkHit {
            end: position,
            captures,
        });
    }
    match &rule.tokens[token_index] {
        Token::Literal { value } => {
            if position + value.len() <= text.len()
                && text[position..position + value.len()] == value[..]
            {
                walk_rule(
                    rule,
                    text,
                    token_index + 1,
                    position + value.len(),
                    captures,
                    dictionaries,
                    separator,
                )
            } else {
                None
            }
        }
        Token::Group {
            alternatives,
            optional,
        } => {
            for value in alternatives {
                if position + value.len() <= text.len()
                    && text[position..position + value.len()] == value[..]
                {
                    if let Some(hit) = walk_rule(
                        rule,
                        text,
                        token_index + 1,
                        position + value.len(),
                        captures.clone(),
                        dictionaries,
                        separator,
                    ) {
                        return Some(hit);
                    }
                }
            }
            if *optional {
                walk_rule(
                    rule,
                    text,
                    token_index + 1,
                    position,
                    captures,
                    dictionaries,
                    separator,
                )
            } else {
                None
            }
        }
        token @ Token::Capture { .. } => {
            for candidate in capture_matches(token, text, position, dictionaries, separator) {
                let mut next_captures = captures.clone();
                let next_pos = position + candidate.len;
                next_captures.push(candidate);
                if let Some(hit) = walk_rule(
                    rule,
                    text,
                    token_index + 1,
                    next_pos,
                    next_captures,
                    dictionaries,
                    separator,
                ) {
                    return Some(hit);
                }
            }
            None
        }
    }
}

/// Try to match one rule at `start` (char index). Returns the hit, if any.
pub fn match_rule(
    rule: &Rule,
    text: &[char],
    start: usize,
    dictionaries: &EngineDicts,
    separator: &str,
) -> Option<MatchHit> {
    let hit = walk_rule(rule, text, 0, start, Vec::new(), dictionaries, separator)?;
    if hit.end <= start {
        return None;
    }
    let end = hit.end;

    // Number-boundary guards: a rule whose first/last token captures a
    // number or year must not continue an adjacent number.
    let is_group_sep = |ch: char| ch == ',' || ch == '，';
    let continues_number_before = start > 0
        && (is_numeric_char(text[start - 1])
            || (start > 1 && is_group_sep(text[start - 1]) && is_numeric_char(text[start - 2])));
    let continues_number_after = end < text.len()
        && (is_numeric_char(text[end])
            || (end + 1 < text.len() && is_group_sep(text[end]) && is_numeric_char(text[end + 1])));

    let first_is_num_capture = matches!(&rule.tokens[0], Token::Capture { sources, .. }
        if sources.iter().any(|s| s == "n" || s == "y"));
    let last_is_num_capture = matches!(rule.tokens.last(), Some(Token::Capture { sources, .. })
        if sources.iter().any(|s| s == "n" || s == "y"));
    if first_is_num_capture && continues_number_before {
        return None;
    }
    if last_is_num_capture && continues_number_after {
        return None;
    }

    let mut translation = rule.translation.clone();
    for (index, capture) in hit.captures.iter().enumerate() {
        translation = translation.replace(&format!("{{{index}}}"), &capture.value);
    }
    let source: String = text[start..end].iter().collect();
    Some(MatchHit {
        start,
        end,
        source,
        translation,
        pattern: rule.pattern.clone(),
        line: rule.line,
    })
}

struct WildcardItem {
    rule: usize,
    anchor: Vec<char>,
    min_offset: usize,
    max_offset: usize,
}

/// Compiled rule set exposing the userscript's `matchAt` operation.
pub struct RuleEngine {
    pub rules: Vec<Rule>,
    pub errors: Vec<RuleError>,
    dictionaries: EngineDicts,
    separator: String,
    vp_max_len: usize,
    direct: HashMap<char, Vec<usize>>,
    wildcard_first: Vec<usize>,
    wildcard_index: HashMap<char, Vec<WildcardItem>>,
    max_wildcard_prefix: usize,
}

impl RuleEngine {
    pub fn new(
        rules: Vec<Rule>,
        errors: Vec<RuleError>,
        dictionaries: EngineDicts,
        separator: String,
        vp_max_len: usize,
    ) -> Self {
        let mut engine = Self {
            rules,
            errors,
            dictionaries,
            separator,
            vp_max_len,
            direct: HashMap::new(),
            wildcard_first: Vec::new(),
            wildcard_index: HashMap::new(),
            max_wildcard_prefix: 0,
        };
        engine.build_index();
        engine
    }

    fn build_index(&mut self) {
        for (order, rule) in self.rules.iter().enumerate() {
            // `order` is the rule's position in the specificity-sorted list.
            let first = &rule.tokens[0];
            match first {
                Token::Literal { value } if !value.is_empty() => {
                    self.direct.entry(value[0]).or_default().push(order);
                }
                Token::Group {
                    alternatives,
                    optional,
                } if !optional => {
                    for alternative in alternatives {
                        if let Some(first_char) = alternative.first() {
                            self.direct.entry(*first_char).or_default().push(order);
                        }
                    }
                }
                _ => {
                    self.wildcard_first.push(order);
                    let mut min_offset = 0;
                    let mut max_offset = 0;
                    let mut anchors: Option<Vec<Vec<char>>> = None;
                    for token in &rule.tokens {
                        match token {
                            Token::Literal { value } if !value.is_empty() => {
                                anchors = Some(vec![value.clone()]);
                                break;
                            }
                            Token::Group {
                                alternatives,
                                optional,
                            } if !optional => {
                                anchors = Some(alternatives.clone());
                                break;
                            }
                            Token::Capture { min, max, .. } => {
                                min_offset += *min;
                                max_offset += *max;
                            }
                            Token::Group { alternatives, .. } => {
                                max_offset += alternatives.iter().map(Vec::len).max().unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                    if let Some(anchors) = anchors {
                        self.max_wildcard_prefix = self.max_wildcard_prefix.max(max_offset);
                        for anchor in anchors {
                            if let Some(first_char) = anchor.first() {
                                self.wildcard_index.entry(*first_char).or_default().push(
                                    WildcardItem {
                                        rule: order,
                                        anchor,
                                        min_offset,
                                        max_offset,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        // The rule's stored `order` must match its sorted position.
        for (order, rule) in self.rules.iter_mut().enumerate() {
            rule.order = order;
        }
    }

    fn longest_vp_at(&self, text: &[char], start: usize) -> usize {
        let maximum = (text.len() - start).min(self.vp_max_len);
        let mut length = maximum;
        while length > 0 {
            let candidate: String = text[start..start + length].iter().collect();
            if self.dictionaries.vp.contains_key(&candidate) {
                return length;
            }
            length -= 1;
        }
        0
    }

    /// Find the first rule matching at `start` (char index), or `None`.
    pub fn match_at(&self, text: &[char], start: usize) -> Option<MatchHit> {
        if start >= text.len() {
            return None;
        }
        let mut candidates: Vec<usize> = self.direct.get(&text[start]).cloned().unwrap_or_default();
        let mut seen: HashSet<usize> = candidates.iter().copied().collect();
        let lookahead_end = (text.len() - start - 1).min(self.max_wildcard_prefix);
        for offset in 1..=lookahead_end {
            let Some(indexed) = self.wildcard_index.get(&text[start + offset]) else {
                continue;
            };
            for item in indexed {
                if offset < item.min_offset || offset > item.max_offset || seen.contains(&item.rule)
                {
                    continue;
                }
                if text.len() < start + offset + item.anchor.len()
                    || text[start + offset..start + offset + item.anchor.len()] != item.anchor[..]
                {
                    continue;
                }
                seen.insert(item.rule);
                candidates.push(item.rule);
            }
        }
        // Defensive fallback for a future wildcard-only syntax. Current
        // validation requires a literal anchor, so this normally stays empty.
        if self.wildcard_index.is_empty() {
            candidates.extend(self.wildcard_first.iter().copied());
        }
        candidates.sort_unstable();

        for rule_order in candidates {
            let rule = &self.rules[rule_order];
            let Some(hit) = match_rule(rule, text, start, &self.dictionaries, &self.separator)
            else {
                continue;
            };
            // VP-overwrite protection: a rule must not shadow a longer
            // plain Vietphrase match starting at the same position.
            if self.longest_vp_at(text, start) >= hit.end - hit.start {
                continue;
            }
            return Some(hit);
        }
        None
    }
}

/// Compile `rule.txt` into an engine, mirroring `VPRuleEngine.create`.
pub fn create_rule_engine(
    rule_text: &str,
    dictionaries: EngineDicts,
    separator: &str,
    vp_max_len: usize,
) -> RuleEngine {
    let parsed = parse_rules(rule_text);
    RuleEngine::new(
        parsed.rules,
        parsed.errors,
        dictionaries,
        separator.to_string(),
        vp_max_len.max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_to_latin_converts_chinese_digits() {
        assert_eq!(year_to_latin("二零二四").as_deref(), Some("2024"));
        assert_eq!(year_to_latin("一九九九").as_deref(), Some("1999"));
        assert_eq!(year_to_latin("二零二四年"), None);
        assert_eq!(year_to_latin("abc"), None);
    }

    #[test]
    fn chinese_to_latin_converts_numerals() {
        assert_eq!(chinese_to_latin("一百二十三").as_deref(), Some("123"));
        assert_eq!(chinese_to_latin("一千二百三十四").as_deref(), Some("1234"));
        assert_eq!(chinese_to_latin("十").as_deref(), Some("10"));
        assert_eq!(chinese_to_latin("二十").as_deref(), Some("20"));
        assert_eq!(
            chinese_to_latin("一万二千三百四十五").as_deref(),
            Some("12345")
        );
        // Pure ASCII digits: `str(int(raw))` drops leading zeros.
        assert_eq!(chinese_to_latin("007").as_deref(), Some("7"));
        assert_eq!(chinese_to_latin("42").as_deref(), Some("42"));
    }

    #[test]
    fn first_meaning_takes_first_segment() {
        assert_eq!(first_meaning("a/b/c", "/"), "a");
        assert_eq!(first_meaning("a", "/"), "a");
        assert_eq!(first_meaning("a¦b", "/"), "a");
    }

    fn empty_dicts() -> EngineDicts {
        EngineDicts {
            pa: HashMap::new(),
            vp: HashMap::new(),
            names: HashMap::new(),
            pronouns: HashMap::new(),
        }
    }

    #[test]
    fn literal_rule_matches_and_substitutes() {
        // A rule needs an anchor (literal) AND a capture; the translation
        // references the capture as `{0}`. The `<n>` source matches a
        // Chinese numeral without any dictionary (`chinese_to_latin`).
        let parsed = parse_rules("第<n>章 = Chương {0}\n");
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let engine = RuleEngine::new(
            parsed.rules,
            parsed.errors,
            empty_dicts(),
            "/".to_string(),
            1,
        );
        let hit = engine.match_at(&['第', '三', '章'], 0);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().translation, "Chương 3");
    }

    #[test]
    fn wildcard_rule_captures_one_char() {
        // The `<vp>` source looks the captured word up in the VP
        // dictionary; the capture value is the first meaning.
        let parsed = parse_rules("他<vp:1>了 = hắn {0} rồi\n");
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        let mut dicts = empty_dicts();
        dicts.vp.insert("说".to_string(), "nói".to_string());
        let engine = RuleEngine::new(parsed.rules, parsed.errors, dicts, "/".to_string(), 1);
        let hit = engine.match_at(&['他', '说', '了'], 0);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().translation, "hắn nói rồi");
    }
}
