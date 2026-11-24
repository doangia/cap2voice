//! Translation + TTS audio caches (A3 / gh #2).
//!
//! Two caches sit at the **completed-sentence** level (after the
//! `SentenceBuffer` finalizes a sentence); the preliminary translation never
//! consults them:
//!
//! * [`TranslationCache`] — key: normalized Chinese text; value: Vietnamese.
//! * [`TtsCache`] — key: (Vietnamese, voice, rate); value: audio bytes in
//!   the engine's format (MP3 for Edge-TTS, WAV for Piper).
//!
//! Both are in-memory LRU (bounded) plus disk persistence under
//! `%APPDATA%\cap2voice\cache\` (see `config::default_cache_dir`), so
//! repeats work across restarts. Eviction removes the oldest entry first;
//! for audio it deletes the audio file as well as the index entry. Keys are
//! exact matches only — no fuzzy cache matching (the dedupe step already
//! handles near-duplicates).
//!
//! Disk layout (inside the cache dir):
//! * `translations.json` — `{normalized_chinese: vietnamese}`.
//! * `tts.json` — `{cache_key: audio_filename}` (index only; bytes live in files).
//! * `audio/<fnv1a-hex>.mp3` / `audio/<fnv1a-hex>.wav` — one audio file per
//!   cached TTS entry, in the engine's format (the extension carries the
//!   format; the voice in the key already separates Edge from Piper entries).

use crate::audio::AudioFormat;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use thiserror::Error;

/// Maximum translation-cache entries (in memory and on disk).
pub const MAX_TRANSLATION_ENTRIES: usize = 500;

/// Maximum total TTS audio bytes held (in memory and on disk).
pub const MAX_TTS_BYTES: u64 = 50 * 1024 * 1024;

/// Translation cache index filename inside the cache dir.
pub const TRANSLATION_INDEX_FILE: &str = "translations.json";

/// TTS cache index filename inside the cache dir.
pub const TTS_INDEX_FILE: &str = "tts.json";

/// Subdirectory holding cached MP3 files.
pub const TTS_AUDIO_DIR: &str = "audio";

/// Typed cache failures. Disk errors are surfaced, never silently swallowed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CacheError {
    #[error("không ghi được bộ nhớ đệm tại {path}: {detail}")]
    WriteFailed { path: PathBuf, detail: String },
}

/// Normalize a subtitle text into a cache key.
///
/// * Trims surrounding whitespace.
/// * Converts fullwidth ASCII / punctuation to halfwidth (`Ａ` → `A`,
///   `，。` → `,.`) and the fullwidth space `U+3000` to a regular space, so
///   fullwidth and halfwidth spellings hit the same entry.
/// * Collapses runs of whitespace to a single space.
/// * Strips trailing sentence-final punctuation (`。？！…` and `. ! ? ，、；：`)
///   so `是。` and `是` hit the same entry where safe.
///
/// Returns an empty string when nothing remains (callers must not cache
/// empty keys).
pub fn normalize_cache_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = true; // trims leading whitespace
    for ch in text.chars() {
        let half = fullwidth_to_halfwidth(ch);
        if half.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(half);
            last_was_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    while out.chars().last().is_some_and(is_sentence_final_punct) {
        out.pop();
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Map a fullwidth ASCII / punctuation code point to its halfwidth form.
/// Fullwidth space (`U+3000`) maps to a regular space. Everything else is
/// returned unchanged.
fn fullwidth_to_halfwidth(ch: char) -> char {
    let code = ch as u32;
    if code == 0x3000 {
        ' '
    } else if (0xFF01..=0xFF5E).contains(&code) {
        char::from_u32(code - 0xFEE0).unwrap_or(ch)
    } else {
        ch
    }
}

/// Sentence-final (or clause-final) punctuation stripped from the end of a
/// cache key. Both fullwidth and halfwidth forms.
fn is_sentence_final_punct(ch: char) -> bool {
    matches!(
        ch,
        '。' | '？' | '！' | '…' | '.' | '!' | '?' | '，' | '、' | '；' | '：' | ',' | ';' | ':'
    )
}

/// Deterministic 64-bit FNV-1a hash, used to derive stable audio filenames
/// from TTS cache keys (the std `DefaultHasher` is randomly seeded per
/// process, so it cannot be used for filenames that must survive restarts).
fn fnv1a_hex(data: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Minimal in-memory LRU: a map plus an access-order queue (oldest at the
/// front). `get` refreshes recency; `insert` evicts the oldest entries
/// while `total_bytes` exceeds `max_bytes` (or the entry count exceeds
/// `max_entries` when `max_bytes` is `u64::MAX`).
struct Lru<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    sizes: HashMap<K, u64>,
    total_bytes: u64,
}

impl<K, V> Lru<K, V>
where
    K: Clone + Eq + std::hash::Hash,
{
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            sizes: HashMap::new(),
            total_bytes: 0,
        }
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        if self.map.contains_key(key) {
            self.touch(key);
        }
        self.map.get(key)
    }

    fn touch(&mut self, key: &K) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
    }

    /// Insert, returning the evicted entries (oldest first) so the caller
    /// can clean up side effects (e.g. delete audio files). The
    /// just-inserted key is never evicted by its own insert, even when a
    /// single entry exceeds `max_bytes` (a single oversize entry is kept;
    /// the next insert evicts it first).
    fn insert(
        &mut self,
        key: K,
        value: V,
        size: u64,
        max_entries: usize,
        max_bytes: u64,
    ) -> Vec<(K, V)> {
        if self.map.contains_key(&key) {
            if let Some(old_size) = self.sizes.insert(key.clone(), size) {
                self.total_bytes = self
                    .total_bytes
                    .saturating_sub(old_size)
                    .saturating_add(size);
            } else {
                self.total_bytes = self.total_bytes.saturating_add(size);
            }
            let _ = self.map.insert(key.clone(), value);
            self.touch(&key);
            return Vec::new();
        }
        self.map.insert(key.clone(), value);
        self.sizes.insert(key.clone(), size);
        self.total_bytes = self.total_bytes.saturating_add(size);
        self.order.push_back(key.clone());
        let mut evicted = Vec::new();
        while self.map.len() > max_entries || self.total_bytes > max_bytes {
            let Some(oldest) = self.order.front().cloned() else {
                break;
            };
            if oldest == key {
                break;
            }
            let _ = self.order.pop_front();
            if let Some(old_value) = self.map.remove(&oldest) {
                if let Some(old_size) = self.sizes.remove(&oldest) {
                    self.total_bytes = self.total_bytes.saturating_sub(old_size);
                }
                evicted.push((oldest, old_value));
            }
        }
        evicted
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.sizes.clear();
        self.total_bytes = 0;
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn entries(&self) -> Vec<(K, V)>
    where
        V: Clone,
    {
        self.order
            .iter()
            .filter_map(|k| self.map.get(k).map(|v| (k.clone(), v.clone())))
            .collect()
    }
}

/// Serializable on-disk index for the translation cache.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TranslationIndex {
    #[serde(default)]
    entries: HashMap<String, String>,
}

/// Translation cache: engine-scoped normalized Chinese → Vietnamese,
/// in-memory LRU plus a `translations.json` index under the cache dir.
/// Keys are `{engine}|{normalized_chinese}` (`helsinki|…` /
/// `vietphrase|…`) so switching engines never serves the other engine's
/// translation. Legacy unprefixed entries (written before the engine
/// option existed) are dropped on load — a one-time migration.
pub struct TranslationCache {
    dir: PathBuf,
    inner: Mutex<Lru<String, String>>,
}

impl TranslationCache {
    /// Load the cache from `dir` (creating the dir when missing). A corrupt
    /// index file is treated as empty (the file is left in place; the next
    /// insert rewrites it).
    pub fn load(dir: &Path) -> Result<Self, CacheError> {
        let dir = dir.to_path_buf();
        fs::create_dir_all(&dir).map_err(|e| CacheError::WriteFailed {
            path: dir.clone(),
            detail: e.to_string(),
        })?;
        let mut lru = Lru::new();
        let index_path = dir.join(TRANSLATION_INDEX_FILE);
        if let Ok(bytes) = fs::read(&index_path) {
            if let Ok(index) = serde_json::from_slice::<TranslationIndex>(&bytes) {
                for (key, value) in index.entries {
                    // One-time migration (D1+A5): entries written before the
                    // engine option existed have no `engine|` prefix and
                    // cannot be attributed to an engine, so drop them.
                    if !key.contains('|') {
                        continue;
                    }
                    let size = (key.len() + value.len()) as u64;
                    let _ = lru.insert(key, value, size, MAX_TRANSLATION_ENTRIES, u64::MAX);
                }
            }
            // A corrupt index is treated as empty; the next insert rewrites it.
        }
        Ok(Self {
            dir,
            inner: Mutex::new(lru),
        })
    }

    /// Compose the engine-scoped cache key (`{engine}|{normalized}`).
    pub fn cache_key(chinese: &str, engine: &str) -> String {
        format!("{engine}|{}", normalize_cache_key(chinese))
    }

    /// Look up a translation by raw Chinese text and engine id. The key is
    /// normalized internally (same as `put`), so `是。` and `是` hit the
    /// same entry — but only within the same engine.
    pub fn get(&self, chinese: &str, engine: &str) -> Option<String> {
        let key = Self::cache_key(chinese, engine);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.get(&key).cloned()
    }

    /// Store a translation (normalizing the key first). Empty keys are
    /// ignored. Persists the index to disk.
    pub fn put(&self, chinese: &str, vietnamese: String, engine: &str) -> Result<(), CacheError> {
        let key = Self::cache_key(chinese, engine);
        if normalize_cache_key(chinese).is_empty() {
            return Ok(());
        }
        let size = (key.len() + vietnamese.len()) as u64;
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let _ = inner.insert(key, vietnamese, size, MAX_TRANSLATION_ENTRIES, u64::MAX);
        }
        self.persist()
    }

    /// Number of entries currently held in memory.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove every entry (memory + disk index file).
    pub fn clear(&self) -> Result<(), CacheError> {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.clear();
        }
        let index_path = self.dir.join(TRANSLATION_INDEX_FILE);
        if index_path.exists() {
            fs::remove_file(&index_path).map_err(|e| CacheError::WriteFailed {
                path: index_path,
                detail: e.to_string(),
            })?;
        }
        Ok(())
    }

    fn persist(&self) -> Result<(), CacheError> {
        let entries: HashMap<String, String> = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.entries().into_iter().collect()
        };
        let index = TranslationIndex { entries };
        let bytes = serde_json::to_vec(&index).map_err(|e| CacheError::WriteFailed {
            path: self.dir.join(TRANSLATION_INDEX_FILE),
            detail: e.to_string(),
        })?;
        fs::write(self.dir.join(TRANSLATION_INDEX_FILE), bytes).map_err(|e| {
            CacheError::WriteFailed {
                path: self.dir.join(TRANSLATION_INDEX_FILE),
                detail: e.to_string(),
            }
        })
    }
}

/// Serializable on-disk index for the TTS cache: cache key → audio filename.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TtsIndex {
    #[serde(default)]
    entries: HashMap<String, String>,
}

/// One cached TTS clip: the raw audio bytes plus their encoding. The
/// format is part of the value (not the key) because the voice in the key
/// already separates engines; the filename extension on disk carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedAudio {
    pub bytes: Vec<u8>,
    pub format: AudioFormat,
}

impl CachedAudio {
    pub fn new(bytes: Vec<u8>, format: AudioFormat) -> Self {
        Self { bytes, format }
    }
}

/// TTS audio cache: (Vietnamese, voice, rate) → audio bytes, in-memory LRU
/// (bounded by [`MAX_TTS_BYTES`]) plus audio files under `audio/` and a
/// `tts.json` index. Eviction deletes the audio file.
pub struct TtsCache {
    dir: PathBuf,
    inner: Mutex<Lru<String, CachedAudio>>,
}

impl TtsCache {
    /// Load the cache from `dir` (creating the dir and `audio/` when
    /// missing). Index entries whose audio file is gone are dropped. A
    /// corrupt index is treated as empty.
    pub fn load(dir: &Path) -> Result<Self, CacheError> {
        let dir = dir.to_path_buf();
        let audio_dir = dir.join(TTS_AUDIO_DIR);
        fs::create_dir_all(&audio_dir).map_err(|e| CacheError::WriteFailed {
            path: audio_dir.clone(),
            detail: e.to_string(),
        })?;
        let mut lru = Lru::new();
        let index_path = dir.join(TTS_INDEX_FILE);
        if let Ok(bytes) = fs::read(&index_path) {
            if let Ok(index) = serde_json::from_slice::<TtsIndex>(&bytes) {
                for (key, filename) in index.entries {
                    let file_path = audio_dir.join(&filename);
                    if let Ok(audio) = fs::read(&file_path) {
                        // The filename extension carries the format
                        // (`.wav` for Piper, `.mp3` for Edge-TTS).
                        let format = if filename.ends_with(".wav") {
                            AudioFormat::Wav
                        } else {
                            AudioFormat::Mp3
                        };
                        let size = audio.len() as u64;
                        let _ = lru.insert(
                            key,
                            CachedAudio::new(audio, format),
                            size,
                            usize::MAX,
                            MAX_TTS_BYTES,
                        );
                    }
                }
            }
            // A corrupt index is treated as empty; the next insert rewrites it.
        }
        Ok(Self {
            dir,
            inner: Mutex::new(lru),
        })
    }

    /// Composite cache key for a TTS request. The Vietnamese text is
    /// normalized (trimmed, whitespace collapsed); voice and rate are exact.
    pub fn cache_key(vietnamese: &str, voice: &str, rate: &str) -> String {
        format!("{voice}|{rate}|{}", normalize_cache_key(vietnamese))
    }

    /// Audio filename for a cache key in `format`.
    fn audio_filename(key: &str, format: AudioFormat) -> String {
        format!("{}.{}", fnv1a_hex(key), format.extension())
    }

    /// Look up cached audio for a (Vietnamese, voice, rate) request.
    pub fn get(&self, vietnamese: &str, voice: &str, rate: &str) -> Option<CachedAudio> {
        let key = Self::cache_key(vietnamese, voice, rate);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.get(&key).cloned()
    }

    /// Store audio for a request. Empty audio is ignored. Persists the
    /// audio file and the index to disk; evicted entries lose their file.
    pub fn put(
        &self,
        vietnamese: &str,
        voice: &str,
        rate: &str,
        audio: CachedAudio,
    ) -> Result<(), CacheError> {
        if audio.bytes.is_empty() {
            return Ok(());
        }
        let key = Self::cache_key(vietnamese, voice, rate);
        if normalize_cache_key(vietnamese).is_empty() {
            return Ok(());
        }
        let filename = Self::audio_filename(&key, audio.format);
        let file_path = self.dir.join(TTS_AUDIO_DIR).join(&filename);
        let size = audio.bytes.len() as u64;
        let evicted = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.insert(key, audio.clone(), size, usize::MAX, MAX_TTS_BYTES)
        };
        for (old_key, old_audio) in &evicted {
            let old_file = self
                .dir
                .join(TTS_AUDIO_DIR)
                .join(Self::audio_filename(old_key, old_audio.format));
            let _ = fs::remove_file(old_file);
        }
        fs::write(&file_path, &audio.bytes).map_err(|e| CacheError::WriteFailed {
            path: file_path,
            detail: e.to_string(),
        })?;
        self.persist()
    }

    /// Number of audio entries currently held in memory.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total MP3 bytes currently held in memory.
    pub fn total_bytes(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .total_bytes
    }

    /// Remove every entry (memory + all audio files + index file).
    pub fn clear(&self) -> Result<(), CacheError> {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.clear();
        }
        let audio_dir = self.dir.join(TTS_AUDIO_DIR);
        if audio_dir.exists() {
            fs::remove_dir_all(&audio_dir).map_err(|e| CacheError::WriteFailed {
                path: audio_dir.clone(),
                detail: e.to_string(),
            })?;
            fs::create_dir_all(&audio_dir).map_err(|e| CacheError::WriteFailed {
                path: audio_dir,
                detail: e.to_string(),
            })?;
        }
        let index_path = self.dir.join(TTS_INDEX_FILE);
        if index_path.exists() {
            fs::remove_file(&index_path).map_err(|e| CacheError::WriteFailed {
                path: index_path,
                detail: e.to_string(),
            })?;
        }
        Ok(())
    }

    fn persist(&self) -> Result<(), CacheError> {
        let entries: HashMap<String, String> = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner
                .entries()
                .into_iter()
                .map(|(key, audio)| (key.clone(), Self::audio_filename(&key, audio.format)))
                .collect()
        };
        let index = TtsIndex { entries };
        let bytes = serde_json::to_vec(&index).map_err(|e| CacheError::WriteFailed {
            path: self.dir.join(TTS_INDEX_FILE),
            detail: e.to_string(),
        })?;
        fs::write(self.dir.join(TTS_INDEX_FILE), bytes).map_err(|e| CacheError::WriteFailed {
            path: self.dir.join(TTS_INDEX_FILE),
            detail: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_cache_key_trims_and_strips_trailing_punctuation() {
        assert_eq!(normalize_cache_key("  是。 "), "是");
        assert_eq!(normalize_cache_key("是"), "是");
        assert_eq!(normalize_cache_key("你好吗？"), "你好吗");
        assert_eq!(normalize_cache_key("你好吗?"), "你好吗");
    }

    #[test]
    fn normalize_cache_key_folds_fullwidth_to_halfwidth() {
        assert_eq!(normalize_cache_key("ＡＢＣ"), "ABC");
        assert_eq!(normalize_cache_key("１２３"), "123");
        assert_eq!(normalize_cache_key("你好，世界"), "你好,世界");
        assert_eq!(normalize_cache_key("　是　"), "是");
    }

    #[test]
    fn normalize_cache_key_collapses_inner_whitespace() {
        assert_eq!(normalize_cache_key("你  好   吗"), "你 好 吗");
        assert_eq!(normalize_cache_key(""), "");
        assert_eq!(normalize_cache_key("。"), "");
    }

    #[test]
    fn translation_cache_round_trips_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_tr_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TranslationCache::load(&dir).expect("load empty cache");
        assert!(cache.is_empty());
        cache
            .put("是。", "Vâng.".to_string(), "helsinki")
            .expect("put");
        cache
            .put("你好", "Xin chào".to_string(), "helsinki")
            .expect("put");
        assert_eq!(cache.get("是", "helsinki"), Some("Vâng.".to_string()));
        assert_eq!(cache.get("你好", "helsinki"), Some("Xin chào".to_string()));
        assert_eq!(cache.get("没有", "helsinki"), None);
        // Reload from disk: the entries survive a restart.
        let reloaded = TranslationCache::load(&dir).expect("reload cache");
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.get("是。", "helsinki"), Some("Vâng.".to_string()));
        assert_eq!(
            reloaded.get("你好", "helsinki"),
            Some("Xin chào".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn translation_cache_evicts_oldest_first() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_evict_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TranslationCache::load(&dir).expect("load empty cache");
        // Fill past the cap by inserting directly into the LRU via put.
        for i in 0..(MAX_TRANSLATION_ENTRIES + 10) {
            cache
                .put(&format!("句子{i}"), format!("Câu {i}"), "helsinki")
                .expect("put");
        }
        assert_eq!(cache.len(), MAX_TRANSLATION_ENTRIES);
        // The first 10 inserted are gone (oldest first); the last one stays.
        assert_eq!(cache.get("句子0", "helsinki"), None);
        assert_eq!(cache.get("句子9", "helsinki"), None);
        assert_eq!(
            cache.get(&format!("句子{}", MAX_TRANSLATION_ENTRIES + 9), "helsinki"),
            Some(format!("Câu {}", MAX_TRANSLATION_ENTRIES + 9))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn translation_cache_clear_removes_disk_index() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_clear_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TranslationCache::load(&dir).expect("load empty cache");
        cache
            .put("是", "Vâng.".to_string(), "helsinki")
            .expect("put");
        assert!(dir.join(TRANSLATION_INDEX_FILE).exists());
        cache.clear().expect("clear");
        assert!(cache.is_empty());
        assert!(!dir.join(TRANSLATION_INDEX_FILE).exists());
        let reloaded = TranslationCache::load(&dir).expect("reload after clear");
        assert!(reloaded.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn translation_cache_keys_are_engine_scoped() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_eng_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TranslationCache::load(&dir).expect("load empty cache");
        cache
            .put("你好", "Xin chào".to_string(), "helsinki")
            .expect("put helsinki");
        cache
            .put("你好", "Ngươi tốt".to_string(), "vietphrase")
            .expect("put vietphrase");
        assert_eq!(cache.get("你好", "helsinki"), Some("Xin chào".to_string()));
        assert_eq!(
            cache.get("你好", "vietphrase"),
            Some("Ngươi tốt".to_string())
        );
        assert_eq!(cache.get("你好", "unknown"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn translation_cache_load_drops_legacy_unprefixed_entries() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_legacy_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create dir");
        // A pre-engine index: one legacy unprefixed entry, one engine-scoped.
        std::fs::write(
            dir.join(TRANSLATION_INDEX_FILE),
            r#"{"entries":{"你好":"Xin chào","helsinki|再见":"Tạm biệt"}}"#,
        )
        .expect("write legacy index");
        let cache = TranslationCache::load(&dir).expect("load legacy index");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get("你好", "helsinki"), None);
        assert_eq!(cache.get("再见", "helsinki"), Some("Tạm biệt".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tts_cache_round_trips_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_tts_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TtsCache::load(&dir).expect("load empty cache");
        assert!(cache.is_empty());
        let mp3 = vec![0x49, 0x44, 0x33, 1, 2, 3];
        cache
            .put(
                "Vâng.",
                "vi-VN-NamMinhNeural",
                "+0%",
                CachedAudio::new(mp3.clone(), AudioFormat::Mp3),
            )
            .expect("put");
        assert_eq!(
            cache.get("Vâng.", "vi-VN-NamMinhNeural", "+0%"),
            Some(CachedAudio::new(mp3.clone(), AudioFormat::Mp3))
        );
        // A different voice is a different key.
        assert_eq!(cache.get("Vâng.", "vi-VN-HoaiMyNeural", "+0%"), None);
        // Reload from disk: the audio survives a restart.
        let reloaded = TtsCache::load(&dir).expect("reload cache");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded.get("Vâng.", "vi-VN-NamMinhNeural", "+0%"),
            Some(CachedAudio::new(mp3, AudioFormat::Mp3))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tts_cache_round_trips_wav_with_wav_extension_on_disk() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_tts_wav_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TtsCache::load(&dir).expect("load empty cache");
        let wav = vec![b'R', b'I', b'F', b'F', 1, 2, 3, 4];
        cache
            .put(
                "Xin chào.",
                "ngochuyen",
                "+0%",
                CachedAudio::new(wav.clone(), AudioFormat::Wav),
            )
            .expect("put wav");
        assert_eq!(
            cache.get("Xin chào.", "ngochuyen", "+0%"),
            Some(CachedAudio::new(wav.clone(), AudioFormat::Wav))
        );
        // The file on disk carries the .wav extension (the format is
        // recoverable from the filename after a restart).
        let audio_dir = dir.join(TTS_AUDIO_DIR);
        let names: Vec<String> = std::fs::read_dir(&audio_dir)
            .expect("read audio dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(
            names[0].ends_with(".wav"),
            "expected a .wav file, got {:?}",
            names
        );
        let reloaded = TtsCache::load(&dir).expect("reload cache");
        assert_eq!(
            reloaded.get("Xin chào.", "ngochuyen", "+0%"),
            Some(CachedAudio::new(wav, AudioFormat::Wav))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tts_cache_eviction_deletes_audio_file() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_tts_evict_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TtsCache::load(&dir).expect("load empty cache");
        // Two entries whose combined size exceeds the cap only when the
        // third arrives; use a small local cap via direct LRU behaviour by
        // inserting entries just under the cap then one more.
        let big = vec![0u8; (MAX_TTS_BYTES / 2 + 1) as usize];
        cache
            .put(
                "Câu một.",
                "vi-VN-NamMinhNeural",
                "+0%",
                CachedAudio::new(big.clone(), AudioFormat::Mp3),
            )
            .expect("put first");
        cache
            .put(
                "Câu hai.",
                "vi-VN-NamMinhNeural",
                "+0%",
                CachedAudio::new(big.clone(), AudioFormat::Mp3),
            )
            .expect("put second");
        // The first entry must be evicted (oldest) and its file deleted.
        assert_eq!(cache.get("Câu một.", "vi-VN-NamMinhNeural", "+0%"), None);
        assert_eq!(
            cache.get("Câu hai.", "vi-VN-NamMinhNeural", "+0%"),
            Some(CachedAudio::new(big, AudioFormat::Mp3))
        );
        let audio_dir = dir.join(TTS_AUDIO_DIR);
        let remaining: Vec<_> = std::fs::read_dir(&audio_dir)
            .expect("read audio dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(remaining.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tts_cache_clear_removes_audio_files_and_index() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_cache_test_tts_clear_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cache = TtsCache::load(&dir).expect("load empty cache");
        cache
            .put(
                "Vâng.",
                "vi-VN-NamMinhNeural",
                "+0%",
                CachedAudio::new(vec![1, 2, 3], AudioFormat::Mp3),
            )
            .expect("put");
        assert!(dir.join(TTS_INDEX_FILE).exists());
        cache.clear().expect("clear");
        assert!(cache.is_empty());
        assert!(!dir.join(TTS_INDEX_FILE).exists());
        let audio_dir = dir.join(TTS_AUDIO_DIR);
        let remaining: Vec<_> = std::fs::read_dir(&audio_dir)
            .expect("read audio dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(remaining.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fnv1a_hex_is_stable_and_distinct() {
        assert_eq!(fnv1a_hex("a"), fnv1a_hex("a"));
        assert_ne!(fnv1a_hex("a"), fnv1a_hex("b"));
        assert_eq!(fnv1a_hex("").len(), 16);
    }
}
