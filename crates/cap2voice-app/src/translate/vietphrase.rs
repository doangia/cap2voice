//! Vietphrase dictionary translator ported from `vietphrase.user.js`
//! (via `server/vietphrase/translator.py`).
//!
//! DOM-free translation pipeline: dictionary parsing, punctuation
//! normalisation, name/rule segmentation, longest VP matching,
//! placeholder resolution, casing and spacing. The browser-only parts of
//! the userscript (MutationObserver, DOM rewriting, clipboard, dictionary
//! manager UI) are deliberately excluded; callers feed text in and get
//! text plus a token map out.
//!
//! The source side works on `Vec<char>` (char indices), mirroring
//! Python's `str` indexing by code point; the translated-output side
//! works on `String`s.

use crate::translate::rule_engine::{create_rule_engine, split_lines, EngineDicts, RuleEngine};
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

/// Han (CJK) character ranges (Python `CHINESE_RE`).
pub fn is_han_char(ch: char) -> bool {
    matches!(ch,
'\u{3400}'..='\u{4dbf}'
| '\u{4e00}'..='\u{9fff}'
| '\u{f900}'..='\u{faff}'
| '\u{3007}')
}

/// Function words dropped when not matched by the VP dictionary
/// (`dichlieu` behaviour).
const DICH_LIEU_SET: [char; 4] = ['的', '了', '着', '著'];

/// Default dictionary directory: `%APPDATA%\cap2voice\vietphrase\data`.
/// Override with `CAP2VOICE_VP_DATA_DIR` (tests, power users).
/// The dictionaries are GPL-3.0 upstream data and are never bundled with
/// the repo; see `server/vietphrase/README.md` in the pre-D1 tree.
pub fn default_dict_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("CAP2VOICE_VP_DATA_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Err("APPDATA environment variable is not set".to_string());
    };
    Ok(PathBuf::from(appdata)
        .join("cap2voice")
        .join("vietphrase")
        .join("data"))
}

const DICT_FILENAMES: [(&str, &str); 5] = [
    ("pa", "ChinesePhienAmWords.txt"),
    ("vp", "Vietphrase.txt"),
    ("names", "Names.txt"),
    ("pronouns", "Pronouns.txt"),
    ("rules", "rule.txt"),
];

/// Punctuation normalisation map, in application order (Python `PUNCT_MAP`).
/// `\x02`/`\x03` are quote placeholders, resolved back at the end.
const PUNCT_MAP: &[(&str, &str)] = &[
    ("。》", ".\u{3}"),
    ("。』", ".\u{3}"),
    ("。」", ".\u{3}"),
    ("？》", "?\u{3}"),
    ("！》", "!\u{3}"),
    ("《", "\u{2}"),
    ("》", "\u{3}"),
    ("〈", "\u{2}"),
    ("〉", "\u{3}"),
    ("「", "\u{2}"),
    ("」", "\u{3}"),
    ("『", "\u{2}"),
    ("』", "\u{3}"),
    ("“", "\u{2}"),
    ("”", "\u{3}"),
    ("‘", "\u{4}"),
    ("’", "\u{5}"),
    ("【", "["),
    ("】", "]"),
    ("〔", "["),
    ("〕", "]"),
    ("〖", "["),
    ("〗", "]"),
    ("（", "("),
    ("）", ")"),
    ("｛", "{"),
    ("｝", "}"),
    ("。", "."),
    ("！", "!"),
    ("？", "?"),
    ("；", ";"),
    ("：", ":"),
    ("，", ","),
    ("、", ", "),
    ("……", "..."),
    ("…", "..."),
    ("——", "—"),
    ("—", "—"),
    ("－", "-"),
    ("～", "~"),
    ("•", "·"),
    ("　", " "),
    ("／", "/"),
    ("＼", "\\"),
    ("！", "!"),
    ("＂", "\""),
    ("＃", "#"),
    ("＄", "$"),
    ("％", "%"),
    ("＆", "&"),
    ("＇", "'"),
    ("＊", "*"),
    ("＋", "+"),
    ("＜", "<"),
    ("＝", "="),
    ("＞", ">"),
    ("＠", "@"),
    ("［", "["),
    ("］", "]"),
    ("＾", "^"),
    ("＿", "_"),
    ("｀", "`"),
    ("｜", "|"),
];

/// Vietnamese-letter test (Python `VIET_RE`).
fn is_viet_char(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ('À'..='ỹ').contains(&ch)
}

/// First-char test for the join rule (Python `CLOSE_RE`).
fn is_close_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{3}' | ',' | '.' | ':' | ';' | '!' | '?' | ')' | '»' | ']'
    )
}

/// Last-char test for the join rule (Python `OPEN_RE` on a single char).
fn is_open_char(ch: char) -> bool {
    matches!(ch, '\u{2}' | '(' | '[') || ch.is_whitespace()
}

/// Sentence-initial uppercase test (Python `_ASCII_UPPER_RE`).
fn is_ascii_upper(ch: char) -> bool {
    ch.is_ascii_uppercase() || ('À'..='Ỹ').contains(&ch) || ch == 'Đ'
}

fn fix_vietnamese(text: &str) -> String {
    text.nfc().collect()
}

fn normalize_dict_object(data: HashMap<String, String>) -> HashMap<String, String> {
    data.into_iter()
        .map(|(k, v)| (fix_vietnamese(&k), fix_vietnamese(&v)))
        .collect()
}

/// Parse an `Hán=Việt` TXT file; `pa_only` allows single Han chars only.
pub fn parse_dictionary(text: &str, pa_only: bool) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for raw in split_lines(text) {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with("//")
            || line.starts_with('#')
            || line.starts_with('=')
        {
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };
        if eq < 1 {
            continue;
        }
        let key = fix_vietnamese(line[..eq].trim());
        let value = fix_vietnamese(line[eq + 1..].trim());
        if key.is_empty() || value.is_empty() {
            continue;
        }
        if pa_only {
            let mut chars = key.chars();
            let Some(first) = chars.next() else {
                continue;
            };
            if chars.next().is_some() || !is_han_char(first) {
                continue;
            }
        }
        out.insert(key, value);
    }
    out
}

/// Raw dictionary text for one translator instance.
#[derive(Debug, Clone, Default)]
pub struct DictionarySet {
    pub pa: HashMap<String, String>,
    pub vp: HashMap<String, String>,
    pub names: HashMap<String, String>,
    pub pronouns: HashMap<String, String>,
    pub rules: String,
}

/// Load the five userscript dictionary files from `directory`.
pub fn load_dictionary_set(directory: &Path) -> Result<DictionarySet, String> {
    let read = |name: &str| -> Result<String, String> {
        std::fs::read_to_string(directory.join(name))
            .map_err(|err| format!("cannot read {}: {err}", directory.join(name).display()))
    };
    let mut by_key: HashMap<&str, String> = HashMap::new();
    for (key, filename) in DICT_FILENAMES {
        by_key.insert(key, read(filename)?);
    }
    Ok(DictionarySet {
        pa: parse_dictionary(&by_key["pa"], true),
        vp: parse_dictionary(&by_key["vp"], false),
        names: parse_dictionary(&by_key["names"], false),
        pronouns: parse_dictionary(&by_key["pronouns"], false),
        rules: fix_vietnamese(&by_key["rules"]),
    })
}

/// Load the dictionaries from the default directory. When they are
/// missing, fail loudly with the exact fetch location instead of a bare
/// I/O error.
pub fn load_default_dictionaries() -> Result<DictionarySet, String> {
    let dir = default_dict_dir()?;
    let missing: Vec<&str> = DICT_FILENAMES
        .iter()
        .filter(|(_, name)| !dir.join(name).is_file())
        .map(|(_, name)| *name)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
"Vietphrase dictionaries are missing from {} ({}). They are not bundled with the repo; copy the five TXT files from https://github.com/duongden/script-vietphrase-translator into that directory.",
dir.display(),
missing.join(", ")
));
    }
    load_dictionary_set(&dir)
}

pub fn has_han_char(text: &str) -> bool {
    text.chars().any(is_han_char)
}

fn take_non_han_run(text: &[char], start: usize) -> String {
    let mut end = start;
    while end < text.len() && !is_han_char(text[end]) {
        end += 1;
    }
    text[start..end].iter().collect()
}

fn normalize_punct(text: &str) -> String {
    let mut out = text.to_string();
    for (source, target) in PUNCT_MAP {
        // An empty source would insert `target` at every position
        // (`str::replace("", x)`); skip it defensively.
        if source.is_empty() {
            continue;
        }
        if out.contains(source) {
            out = out.replace(source, target);
        }
    }
    out
}

fn resolve_placeholders(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\u{2}' | '\u{3}' => '"',
            '\u{4}' => '‘',
            '\u{5}' => '’',
            _ => ch,
        })
        .collect()
}

fn join_translated_tokens(tokens: &[String]) -> String {
    let mut result = String::new();
    for token in tokens {
        if token.is_empty() {
            continue;
        }
        if result.is_empty() {
            result = token.clone();
            continue;
        }
        let last = result.chars().last().unwrap_or(' ');
        let first = token.chars().next().unwrap_or(' ');
        if is_close_char(first) || is_open_char(last) || last == ' ' {
            result.push_str(token);
        } else if is_viet_char(last) || is_viet_char(first) || last == '\u{3}' || last == '\u{5}' {
            result.push(' ');
            result.push_str(token);
        } else {
            result.push_str(token);
        }
    }
    result
}

#[derive(Debug, Clone)]
struct Pair {
    han: String,
    viet: String,
    is_name: bool,
}

fn normalize_token_case(pairs: &mut [Pair]) {
    let mut context = String::new();
    for pair in pairs.iter_mut() {
        let value = pair.viet.clone();
        let starts_sentence = context.trim().is_empty()
            || ends_with_sentence_punct(&context)
            || ends_with_open_quote(&context);
        if !starts_sentence && !pair.is_name {
            if let Some(first) = value.chars().next() {
                if is_ascii_upper(first) {
                    let lowered: String = first.to_lowercase().collect();
                    pair.viet = lowered + &value[first.len_utf8()..];
                }
            }
        }
        context = join_translated_tokens(&[context, pair.viet.clone()]);
    }
}

/// `re.search(r"[.!?]\s*$", context)`.
fn ends_with_sentence_punct(context: &str) -> bool {
    let trimmed = context.trim_end();
    trimmed.ends_with('.') || trimmed.ends_with('!') || trimmed.ends_with('?')
}

/// `re.search(r"\x02\s*$", context)`.
fn ends_with_open_quote(context: &str) -> bool {
    let trimmed = context.trim_end();
    trimmed.ends_with('\u{2}')
}

fn post_process_translated_text(text: &str) -> String {
    static SPACES: OnceLock<Regex> = OnceLock::new();
    static R1: OnceLock<Regex> = OnceLock::new();
    static R2: OnceLock<Regex> = OnceLock::new();
    static R3: OnceLock<Regex> = OnceLock::new();
    static R4: OnceLock<Regex> = OnceLock::new();
    static R5: OnceLock<Regex> = OnceLock::new();
    static R6: OnceLock<Regex> = OnceLock::new();
    static R7: OnceLock<Regex> = OnceLock::new();
    let spaces = SPACES.get_or_init(|| Regex::new(r"\s{2,}").unwrap_or_else(|_| unreachable!()));
    let r1 =
        R1.get_or_init(|| Regex::new(r"\s+([\u{3},.:;!?)»\]])").unwrap_or_else(|_| unreachable!()));
    let r2 = R2.get_or_init(|| Regex::new(r"([\u{2}(\[])\s+").unwrap_or_else(|_| unreachable!()));
    let r3 = R3.get_or_init(|| {
        Regex::new(r"(\u{3})([a-zA-ZÀ-ỹ\u{2}(\[])").unwrap_or_else(|_| unreachable!())
    });
    let r4 = R4.get_or_init(|| {
        Regex::new(r"([a-zA-ZÀ-ỹ\u{3}\])])(\u{2})").unwrap_or_else(|_| unreachable!())
    });
    let r5 = R5.get_or_init(|| {
        Regex::new(r"([:;!?,])([a-zA-ZÀ-ỹ\u{2}])").unwrap_or_else(|_| unreachable!())
    });
    let r6 =
        R6.get_or_init(|| Regex::new(r"([a-zA-ZÀ-ỹ])([(\[])").unwrap_or_else(|_| unreachable!()));
    // Python: `(^|[^0-9]),(?=[0-9])` — the `regex` crate has no lookahead,
    // so the digit is consumed and re-emitted; the result is identical
    // (a digit can never serve as the `[^0-9]` of the next match).
    let r7 =
        R7.get_or_init(|| Regex::new(r"(^|[^0-9]),([0-9])").unwrap_or_else(|_| unreachable!()));
    let text = spaces.replace_all(text, " ");
    let text = r1.replace_all(&text, "$1");
    let text = r2.replace_all(&text, "$1");
    let text = r3.replace_all(&text, "$1 $2");
    let text = r4.replace_all(&text, "$1 $2");
    let text = r5.replace_all(&text, "$1 $2");
    let text = r6.replace_all(&text, "$1 $2");
    let text = r7.replace_all(&text, "$1, $2");
    text.trim().to_string()
}

/// Lowercase Vietnamese letters (Python `_LOWER_VI`), for `auto_capitalize`.
const LOWER_VI: &str = "a-zàáâãèéêìíòóôõùúýăđơưạảấầẩẫậắằẳẵặẹẻẽếềểễệỉịọỏốồổỗộớờởỡợụủứừửữựỳỷỹ";

fn upper_first_char(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn auto_capitalize(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    let trimmed = text.trim_start();
    let mut out = upper_first_char(trimmed);
    static R1: OnceLock<Regex> = OnceLock::new();
    static R2: OnceLock<Regex> = OnceLock::new();
    static R3: OnceLock<Regex> = OnceLock::new();
    let r1 = R1.get_or_init(|| {
        Regex::new(&format!(r"([.!?])(\s*)([{LOWER_VI}])")).unwrap_or_else(|_| unreachable!())
    });
    let r2 = R2.get_or_init(|| {
        Regex::new(&format!(r"(:\s+\x02)([{LOWER_VI}])")).unwrap_or_else(|_| unreachable!())
    });
    let r3 = R3.get_or_init(|| {
        Regex::new(&format!(r"((?:^|\s)\x02)([{LOWER_VI}])")).unwrap_or_else(|_| unreachable!())
    });
    out = r1
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}{}{}", &caps[1], &caps[2], upper_first_char(&caps[3]))
        })
        .into_owned();
    out = r2
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}{}", &caps[1], upper_first_char(&caps[2]))
        })
        .into_owned();
    out = r3
        .replace_all(&out, |caps: &regex::Captures<'_>| {
            format!("{}{}", &caps[1], upper_first_char(&caps[2]))
        })
        .into_owned();
    out
}

/// Translation options. DOM-only fields are kept for userscript parity.
#[derive(Debug, Clone)]
pub struct Settings {
    pub ngoac: bool,
    pub motnghia: bool,
    pub daucach: String,
    pub dichlieu: bool,
    pub rule_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ngoac: false,
            motnghia: true,
            daucach: "/".to_string(),
            dichlieu: true,
            rule_enabled: true,
        }
    }
}

/// Alignment between a source (Han) span and its translated (Viet) span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMapEntry {
    pub han: String,
    pub viet: String,
    pub start: usize,
    pub end: usize,
}

/// Translation result: final text + token alignment map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationResult {
    pub result: String,
    pub token_map: Vec<TokenMapEntry>,
}

#[derive(Debug, Clone)]
struct Segment {
    text: String,
    han: String,
    is_name: bool,
    is_rule: bool,
}

fn plain_segment(text: String) -> Segment {
    Segment {
        text,
        han: String::new(),
        is_name: false,
        is_rule: false,
    }
}

/// Dictionary-based Chinese→Vietnamese translator.
pub struct VietPhraseTranslator {
    settings: Settings,
    dict_pa: HashMap<String, String>,
    dict_vp: HashMap<String, String>,
    dict_names: HashMap<String, String>,
    dict_vp_max_len: usize,
    dict_names_max_len: usize,
    rule_engine: Option<RuleEngine>,
}

impl VietPhraseTranslator {
    pub fn new(dictionaries: DictionarySet, settings: Option<Settings>) -> Self {
        let settings = settings.unwrap_or_default();
        let dict_pa = normalize_dict_object(dictionaries.pa);
        let dict_vp = normalize_dict_object(dictionaries.vp);
        let dict_names = normalize_dict_object(dictionaries.names);
        let dict_pronouns = normalize_dict_object(dictionaries.pronouns);
        let rule_text = fix_vietnamese(&dictionaries.rules);

        // Lengths are measured in *characters* (Python `len(str)`),
        // not bytes: VP keys are overwhelmingly CJK.
        let dict_vp_max_len = dict_vp
            .keys()
            .map(|k| k.chars().count())
            .max()
            .unwrap_or(1)
            .max(1);
        let dict_names_max_len = dict_names
            .keys()
            .map(|k| k.chars().count())
            .max()
            .unwrap_or(0);

        let rule_engine = {
            let dicts = EngineDicts {
                pa: dict_pa.clone(),
                vp: dict_vp.clone(),
                names: dict_names.clone(),
                pronouns: dict_pronouns.clone(),
            };
            let engine = create_rule_engine(&rule_text, dicts, &settings.daucach, dict_vp_max_len);
            if engine.rules.is_empty() {
                None
            } else {
                Some(engine)
            }
        };

        Self {
            settings,
            dict_pa,
            dict_vp,
            dict_names,
            dict_vp_max_len,
            dict_names_max_len,
            rule_engine,
        }
    }

    pub fn translate(&self, text: &str) -> String {
        self.translate_with_map(text).result
    }

    pub fn translate_with_map(&self, text: &str) -> TranslationResult {
        let text = fix_vietnamese(text);
        if text.trim().is_empty() || !has_han_char(&text) {
            return TranslationResult {
                result: text,
                token_map: Vec::new(),
            };
        }

        let ngoac = self.settings.ngoac;
        let motnghia = self.settings.motnghia;
        let daucach = self.settings.daucach.clone();
        let dichlieu = self.settings.dichlieu;
        let text = normalize_punct(&text);
        let chars: Vec<char> = text.chars().collect();

        // Rule segmentation (char indices).
        let mut rule_segments: Vec<Segment> = Vec::new();
        if self.settings.rule_enabled {
            if let Some(engine) = &self.rule_engine {
                let mut plain_start = 0;
                let mut cursor = 0;
                while cursor < chars.len() {
                    let Some(hit) = engine.match_at(&chars, cursor) else {
                        cursor += 1;
                        continue;
                    };
                    if plain_start < cursor {
                        rule_segments
                            .push(plain_segment(chars[plain_start..cursor].iter().collect()));
                    }
                    rule_segments.push(Segment {
                        text: hit.translation,
                        han: hit.source,
                        is_name: false,
                        is_rule: true,
                    });
                    cursor = hit.end;
                    plain_start = cursor;
                }
                if plain_start < chars.len() {
                    rule_segments.push(plain_segment(chars[plain_start..].iter().collect()));
                }
            } else {
                rule_segments.push(plain_segment(text.clone()));
            }
        } else {
            rule_segments.push(plain_segment(text.clone()));
        }

        // Name segmentation inside each plain segment.
        let mut segments: Vec<Segment> = Vec::new();
        for segment in rule_segments {
            if segment.is_rule {
                segments.push(segment);
            } else {
                segments.extend(self.take_name_segments(&segment.text));
            }
        }

        // Longest VP match per segment.
        let mut pairs: Vec<Pair> = Vec::new();
        for segment in segments {
            if segment.is_rule {
                pairs.push(Pair {
                    han: segment.han,
                    viet: segment.text,
                    is_name: false,
                });
                continue;
            }
            if segment.is_name {
                pairs.push(Pair {
                    han: segment.han,
                    viet: segment.text,
                    is_name: true,
                });
                continue;
            }
            let source: Vec<char> = segment.text.chars().collect();
            let mut i = 0;
            while i < source.len() {
                let mut matched = false;
                let remaining = source.len() - i;
                let mut length = self.dict_vp_max_len.min(remaining);
                while length >= 1 {
                    let sub: String = source[i..i + length].iter().collect();
                    if let Some(vp) = self.dict_vp.get(&sub) {
                        let mut viet = if motnghia {
                            first_meaning_vp(vp, &daucach)
                        } else {
                            vp.trim().to_string()
                        };
                        if ngoac {
                            viet = format!("[{viet}]");
                        }
                        pairs.push(Pair {
                            han: sub,
                            viet,
                            is_name: false,
                        });
                        i += length;
                        matched = true;
                        break;
                    }
                    if length == 1 {
                        break;
                    }
                    length -= 1;
                }
                if matched {
                    continue;
                }
                let ch = source[i];
                if !is_han_char(ch) {
                    let raw = take_non_han_run(&source, i);
                    if !raw.is_empty() {
                        let raw_len = raw.chars().count();
                        pairs.push(Pair {
                            han: raw.clone(),
                            viet: raw,
                            is_name: false,
                        });
                        i += raw_len.max(1);
                        continue;
                    }
                }
                if dichlieu && DICH_LIEU_SET.contains(&ch) {
                    i += 1;
                    continue;
                }
                pairs.push(Pair {
                    han: ch.to_string(),
                    viet: self
                        .dict_pa
                        .get(&ch.to_string())
                        .cloned()
                        .unwrap_or_else(|| ch.to_string()),
                    is_name: false,
                });
                i += 1;
            }
        }

        normalize_token_case(&mut pairs);
        let mut result = post_process_translated_text(&join_translated_tokens(
            &pairs.iter().map(|p| p.viet.clone()).collect::<Vec<_>>(),
        ));

        let mut token_map: Vec<TokenMapEntry> = Vec::new();
        let mut search_from = 0;
        for pair in &pairs {
            if pair.viet.is_empty() {
                continue;
            }
            let start = match result[search_from..].find(&pair.viet) {
                Some(pos) => search_from + pos,
                None => match result.find(&pair.viet) {
                    Some(pos) => pos,
                    None => continue,
                },
            };
            let end = start + pair.viet.len();
            token_map.push(TokenMapEntry {
                han: pair.han.clone(),
                viet: pair.viet.clone(),
                start,
                end,
            });
            search_from = end;
        }

        result = fix_vietnamese(&resolve_placeholders(&auto_capitalize(&result)));
        for token in token_map.iter_mut() {
            token.viet = result[token.start..token.end].to_string();
        }
        TranslationResult { result, token_map }
    }

    fn take_name_segments(&self, text: &str) -> Vec<Segment> {
        if self.dict_names_max_len == 0 {
            return vec![plain_segment(text.to_string())];
        }
        let chars: Vec<char> = text.chars().collect();
        let mut segments: Vec<Segment> = Vec::new();
        let mut plain_start = 0;
        let mut i = 0;
        while i < chars.len() {
            let mut name = String::new();
            let remaining = chars.len() - i;
            let mut length = self.dict_names_max_len.min(remaining);
            while length >= 1 {
                let candidate: String = chars[i..i + length].iter().collect();
                if self.dict_names.contains_key(&candidate) {
                    name = candidate;
                    break;
                }
                if length == 1 {
                    break;
                }
                length -= 1;
            }
            if name.is_empty() {
                i += 1;
                continue;
            }

            // VP-overwrite guard: if a VP entry covers the name (or more),
            // the name does not win.
            let mut vp_length = 0;
            let mut length = self.dict_vp_max_len.min(remaining);
            while length >= name.chars().count() {
                let candidate: String = chars[i..i + length].iter().collect();
                if self.dict_vp.contains_key(&candidate) {
                    vp_length = length;
                    break;
                }
                if length == name.chars().count() {
                    break;
                }
                length -= 1;
            }
            if vp_length >= name.chars().count() {
                i += 1;
                continue;
            }

            if plain_start < i {
                segments.push(plain_segment(chars[plain_start..i].iter().collect()));
            }
            let mut name_value = self.dict_names[&name].clone();
            name_value = if self.settings.motnghia {
                first_meaning_vp(&name_value, &self.settings.daucach)
            } else {
                name_value.trim().to_string()
            };
            if self.settings.ngoac {
                name_value = format!("[{name_value}]");
            }
            segments.push(Segment {
                text: name_value,
                han: name.clone(),
                is_name: true,
                is_rule: false,
            });
            i += name.chars().count();
            plain_start = i;
        }

        if plain_start < chars.len() {
            segments.push(plain_segment(chars[plain_start..].iter().collect()));
        }
        segments
    }
}

/// First sense of a VP value (`/` split, plus `¦`), mirroring
/// `first_meaning` in the rule engine with the translator's separator.
fn first_meaning_vp(value: &str, separator: &str) -> String {
    crate::translate::rule_engine::first_meaning(value, separator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dictionary_skips_comments_and_blank_lines() {
        let map = parse_dictionary("# comment\n\n你好=Ngươi tốt\n雨=Mưa\n", false);
        assert_eq!(map.get("你好").map(String::as_str), Some("Ngươi tốt"));
        assert_eq!(map.get("雨").map(String::as_str), Some("Mưa"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_dictionary_pa_only_keeps_single_han_char_keys() {
        let map = parse_dictionary("你=ngươi\n你好=Ngươi tốt\n", true);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("你"));
    }

    #[test]
    fn translate_returns_input_for_empty_line() {
        let translator = VietPhraseTranslator::new(
            DictionarySet {
                pa: HashMap::new(),
                vp: HashMap::new(),
                names: HashMap::new(),
                pronouns: HashMap::new(),
                rules: String::new(),
            },
            None,
        );
        assert_eq!(translator.translate(""), "");
        assert_eq!(translator.translate("你好"), "你好");
    }
}
