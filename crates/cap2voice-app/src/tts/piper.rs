//! Piper offline TTS engine (2026-11-24): Vietnamese VITS voices from the
//! NGHI-TTS family, running fully offline beside Edge-TTS.
//!
//! # Implementation direction — (a) sherpa-onnx crate + external phonemization
//!
//! The app uses the `sherpa-onnx` 2.0.3 and `piper-phonemize` 1.6.1 Rust
//! crates, both with the `shared` feature (DLLs). The synthesis path is
//! exactly the one proven by the VM spike (`~/workspace/nghitts-test/`):
//! phonemize the Vietnamese text with piper-phonemize (espeak-ng `vi`
//! voice) FIRST, then pass the resulting Unicode codepoints as
//! `GenerationConfig::phoneme_codepoints` to
//! `OfflineTts::generate_with_config` with an empty text argument.
//!
//! Direction (a) was chosen over (c) (Piper directly via the app's existing
//! `ort` dependency) because the exact token sequence the VITS model expects
//! could NOT be reverse-engineered reliably: length-based matching against
//! the sherpa reference was inconclusive (the best hypothesis matched one
//! sentence within 0.3% but was +11% off on a long sentence, and
//! `length_scale` behaved non-linearly in the tests). Direction (a) is
//! correct by construction — sherpa-onnx's own VITS path does the token
//! framing internally. Direction (b) (pin sherpa v1) was rejected: v1 is
//! unmaintained and its README is the trap below.
//!
//! # Trap — the sherpa-onnx v2 espeak-ng removal
//!
//! The voice repo's README is written for sherpa-onnx v1, but v2.0.x
//! dropped espeak-ng and piper-phonemize (sherpa-onnx PR #4055). Following
//! the README directly fails with "phoneme_codepoints is empty". The fix
//! is the phonemize-first path above — never pass raw text to sherpa
//! without `phoneme_codepoints` (there is no lexicon for Vietnamese).
//!
//! # Why `shared` (DLLs), not static linking
//!
//! Both crates' Windows static libraries are built with `/MT` (static CRT
//! — verified from the `.drectve` sections of the downloaded archives:
//! `RuntimeLibrary=MT_StaticRelease`), while this app and `ort` use `/MD`
//! (dynamic CRT). Statically linking them would fail with LNK2038 — the
//! same conflict that forced dropping `esaxx_fast` from `tokenizers`
//! (see `Cargo.toml`). The `shared` DLLs carry their own CRT, so there is
//! no conflict; the `-sys` build scripts copy the DLLs next to the exe in
//! `target/release/` (`sherpa-onnx-c-api.dll`, `sherpa-onnx-cxx-api.dll`,
//! `onnxruntime.dll`, `piper_phonemize_cxx_api.dll`,
//! `piper_phonemize_core.dll`). The exe must ship with those DLLs beside
//! it. Note sherpa-onnx bundles its OWN `onnxruntime.dll`, separate from
//! the one `ort` loads — they coexist (different modules).
//!
//! # Voice source and license — personal/hobby use only
//!
//! Voices come from `https://huggingface.co/doof-ferb/nghitts-copy`
//! (the `sherpa-onnx/` folder), an UNOFFICIAL mirror of NGHI-TTS Piper
//! VITS voices. Its metadata says Apache-2.0, but the voices are named
//! after real people. The models are downloaded on demand into
//! `%APPDATA%\cap2voice\voices\piper\<voice-id>\` — they are NEVER bundled
//! into the binary or the repo. Additionally, `piper-phonemize` is
//! **GPL-3.0-or-later** (because of espeak-ng) and it embeds the
//! espeak-ng-data archive (~7 MB) into the binary at compile time, so the
//! exe grows by ~7 MB. The combined binary therefore carries GPL-licensed
//! components: cap2voice with Piper is for Danh's personal/hobby use only
//! and must NOT be distributed.
//!
//! # Offline behavior
//!
//! Piper needs no network at synthesis time, so there is no 429 retry
//! (unlike Edge-TTS). A failed synthesis returns `TtsError` directly.
//! The model loads lazily on first use (or when the engine is selected in
//! Settings); if the voice has not been downloaded yet, synthesis fails
//! with `TtsError::EngineUnavailable` telling the user to download it in
//! Settings → Giọng đọc.

use crate::api::{BoxFuture, TtsEngine, TtsError, TtsSynthesis};
use crate::audio::AudioFormat;
use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsModelConfig,
    OfflineTtsVitsModelConfig,
};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

/// Hugging Face repo hosting the Piper VITS voices (unofficial NGHI-TTS
/// mirror — see the module docs; personal/hobby use only).
pub const PIPER_VOICE_REPO: &str = "https://huggingface.co/doof-ferb/nghitts-copy";

/// Shared phoneme-token file (same for every voice in the repo).
pub const PIPER_TOKENS_FILE: &str = "tokens.txt";

/// espeak-ng voice id for Vietnamese phonemization.
const ESPEAK_VOICE_VI: &str = "vi";

/// Sample rate of every voice in the repo (22050 Hz, 1 speaker).
pub const PIPER_SAMPLE_RATE: u32 = 22_050;

/// One downloadable Piper voice. Add a row per voice from the same repo
/// (Mạnh Dũng, Minh Quang, …) — no other code changes are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PiperVoice {
    /// Stable id, used in `config.toml` (`voice.voice`) and as the voice
    /// directory name.
    pub id: &'static str,
    /// Vietnamese UI label.
    pub label: &'static str,
    /// ONNX filename inside the repo's `sherpa-onnx/` folder.
    pub onnx_file: &'static str,
    /// Expected ONNX file size in bytes (download verification).
    pub onnx_bytes: u64,
}

/// Voice registry, in Settings order. Ngọc Huyền first (the voice Danh
/// listened to and approved, 2026-11-24).
pub const PIPER_VOICES: &[PiperVoice] = &[PiperVoice {
    id: "ngochuyen",
    label: "Ngọc Huyền (Piper, offline)",
    onnx_file: "ngochuyen.onnx",
    onnx_bytes: 63_516_173,
}];

/// Default Piper voice id (Ngọc Huyền).
pub const DEFAULT_PIPER_VOICE_ID: &str = "ngochuyen";

/// Find a registered Piper voice by id.
pub fn find_piper_voice(id: &str) -> Option<&'static PiperVoice> {
    PIPER_VOICES.iter().find(|voice| voice.id == id)
}

/// Download URL of a voice's ONNX file.
pub fn piper_onnx_url(voice: &PiperVoice) -> String {
    format!(
        "{PIPER_VOICE_REPO}/resolve/main/sherpa-onnx/{}",
        voice.onnx_file
    )
}

/// Download URL of the shared tokens file.
pub fn piper_tokens_url() -> String {
    format!("{PIPER_VOICE_REPO}/resolve/main/sherpa-onnx/{PIPER_TOKENS_FILE}")
}

/// Default voices directory: `%APPDATA%\cap2voice\voices\piper`.
/// Override with `CAP2VOICE_PIPER_VOICES_DIR` (tests, power users).
/// Mirrors `translate::default_model_dir`.
pub fn default_voices_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("CAP2VOICE_PIPER_VOICES_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Err("APPDATA environment variable is not set".to_string());
    };
    Ok(PathBuf::from(appdata)
        .join("cap2voice")
        .join("voices")
        .join("piper"))
}

/// Directory holding one voice's files (`<voices_dir>/<voice-id>/`).
pub fn voice_dir(voices_dir: &Path, voice_id: &str) -> PathBuf {
    voices_dir.join(voice_id)
}

/// `true` when a voice's ONNX + tokens files are both present on disk.
pub fn voice_files_present(voice: &PiperVoice, voices_dir: &Path) -> bool {
    let dir = voice_dir(voices_dir, voice.id);
    dir.join(voice.onnx_file).is_file() && dir.join(PIPER_TOKENS_FILE).is_file()
}

/// Progress events from a background voice download, delivered to the
/// Settings window (which polls the channel each frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiperDownloadEvent {
    /// Bytes downloaded so far for the ONNX file (`total` is the expected
    /// size when known).
    Progress { downloaded: u64, total: Option<u64> },
    /// The voice finished downloading and verified.
    Done { voice_id: String },
    /// The download failed (message is user-facing Vietnamese).
    Error(String),
}

/// Download one voice (ONNX + tokens) on a background thread, reporting
/// progress on the returned channel. Files are written to `<name>.download`
/// first and renamed only after the size check passes, so a partial
/// download never looks like a complete voice.
pub fn download_voice(
    voice: &'static PiperVoice,
    voices_dir: &Path,
) -> mpsc::Receiver<PiperDownloadEvent> {
    let (tx, rx) = mpsc::sync_channel::<PiperDownloadEvent>(64);
    let dir = voice_dir(voices_dir, voice.id);
    let tx_thread = tx.clone();
    let spawn = std::thread::Builder::new()
        .name(format!("cap2voice-piper-download-{}", voice.id))
        .spawn(move || {
            let result = download_voice_blocking(voice, &dir, &tx_thread);
            let event = match result {
                Ok(()) => PiperDownloadEvent::Done {
                    voice_id: voice.id.to_string(),
                },
                Err(detail) => PiperDownloadEvent::Error(detail),
            };
            let _ = tx_thread.send(event);
        });
    if let Err(err) = spawn {
        let _ = tx.send(PiperDownloadEvent::Error(format!(
            "không tạo được luồng tải giọng: {err}"
        )));
    }
    rx
}

fn download_voice_blocking(
    voice: &PiperVoice,
    dir: &Path,
    tx: &mpsc::SyncSender<PiperDownloadEvent>,
) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("không tạo được thư mục giọng {}: {err}", dir.display()))?;

    // ONNX first (the 63.5 MB file — the only one with progress); the
    // tokens file is 966 bytes and follows silently.
    let onnx_url = piper_onnx_url(voice);
    let onnx_dest = dir.join(voice.onnx_file);
    download_file(
        &onnx_url,
        &onnx_dest,
        Some(voice.onnx_bytes),
        &|downloaded, total| {
            // Progress reporting must never stall the download: the
            // channel is bounded and the UI drains it once per frame, so
            // a full channel drops the event (the next one carries a
            // newer total anyway).
            let _ = tx.try_send(PiperDownloadEvent::Progress { downloaded, total });
        },
    )
    .map_err(|err| format!("không tải được giọng {}: {err}", voice.label))?;

    let tokens_dest = dir.join(PIPER_TOKENS_FILE);
    download_file(&piper_tokens_url(), &tokens_dest, None, &|_, _| {})
        .map_err(|err| format!("không tải được tokens.txt: {err}"))?;

    Ok(())
}

/// Download `url` to `dest` (via `<dest>.download` + rename), verifying the
/// byte count when `expected_bytes` is known. `on_progress` is called
/// after every chunk with the running total.
fn download_file(
    url: &str,
    dest: &Path,
    expected_bytes: Option<u64>,
    on_progress: &dyn Fn(u64, Option<u64>),
) -> Result<(), String> {
    // A per-read timeout (60 s) so a stalled connection fails visibly
    // instead of hanging the download thread forever: the first live
    // run stalled at ~3 MB with no error and no progress (2026-11-24).
    // No global/per-call timeout — a 63.5 MB download on a slow link
    // legitimately takes minutes.
    let agent = ureq::Agent::config_builder()
        .timeout_per_read(Some(std::time::Duration::from_secs(60)))
        .build()
        .new_agent();
    let response = agent
        .get(url)
        .call()
        .map_err(|err| format!("lỗi tải {url}: {err}"))?;
    let total = response.body().content_length().or(expected_bytes);
    let mut reader = response.into_body().into_reader();
    let tmp_path = dest.with_extension("download");
    let mut file = std::fs::File::create(&tmp_path)
        .map_err(|err| format!("không tạo được file tạm {}: {err}", tmp_path.display()))?;
    let mut buf = [0u8; 65_536];
    let mut downloaded: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|err| format!("lỗi đọc dữ liệu tải về: {err}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|err| format!("lỗi ghi file tải về: {err}"))?;
        downloaded = downloaded.saturating_add(n as u64);
        on_progress(downloaded, total);
    }
    drop(file);
    if let Some(expected) = expected_bytes {
        if downloaded != expected {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!(
                "kích thước file tải về không khớp (nhận {downloaded} byte, cần {expected} byte) — thử tải lại"
            ));
        }
    }
    std::fs::rename(&tmp_path, dest)
        .map_err(|err| format!("không lưu được file {}: {err}", dest.display()))?;
    Ok(())
}

/// Map an Edge-TTS speech-rate string (`"+0%"`, `"+10%"`, `"-20%"`) to a
/// Piper speed multiplier (`1.0` = normal, `>1` = faster). An invalid rate
/// never fails synthesis — it falls back to `1.0`. Clamped to `0.5..=2.0`.
pub fn parse_rate_to_speed(rate: &str) -> f32 {
    let trimmed = rate.trim();
    let (negative, rest) = if let Some(rest) = trimmed.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = trimmed.strip_prefix('+') {
        (false, rest)
    } else {
        (false, trimmed)
    };
    let digits = rest.strip_suffix('%').unwrap_or(rest);
    match digits.parse::<f32>() {
        Ok(pct) => {
            let speed = if negative {
                1.0 - pct / 100.0
            } else {
                1.0 + pct / 100.0
            };
            speed.clamp(0.5, 2.0)
        }
        Err(_) => 1.0,
    }
}

/// Encode f32 PCM samples as a WAV file (16-bit PCM, mono). Used for the
/// Piper output so the TTS cache and the `rodio` player handle it like any
/// other audio clip.
pub fn encode_wav_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&sample_rate.saturating_mul(2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// Loaded Piper TTS engine for one voice: the sherpa-onnx VITS session
/// behind a `Mutex` (sherpa marks `OfflineTts` `Send`/`Sync` but documents
/// single-object usage, so synthesis is serialized).
pub struct PiperTtsEngine {
    voice_id: &'static str,
    tts: Arc<Mutex<OfflineTts>>,
}

impl PiperTtsEngine {
    /// Load a downloaded voice from `voices_dir` (`<voices_dir>/<id>/` must
    /// hold the ONNX + tokens files — see `voice_files_present`). Slow
    /// (ONNX session load) — call from a background thread, never on the
    /// UI thread.
    pub fn load(voice: &'static PiperVoice, voices_dir: &Path) -> Result<Self, TtsError> {
        let dir = voice_dir(voices_dir, voice.id);
        let onnx_path = dir.join(voice.onnx_file);
        let tokens_path = dir.join(PIPER_TOKENS_FILE);
        if !onnx_path.is_file() {
            return Err(TtsError::EngineUnavailable {
                detail: format!(
                    "chưa tải giọng Piper '{}' — bấm 'Tải giọng' trong Cài đặt → Giọng đọc",
                    voice.label
                ),
            });
        }
        if !tokens_path.is_file() {
            return Err(TtsError::EngineUnavailable {
                detail: format!(
                    "thiếu file tokens của giọng Piper '{}' — tải lại giọng trong Cài đặt → Giọng đọc",
                    voice.label
                ),
            });
        }
        let model = OfflineTtsModelConfig {
            vits: OfflineTtsVitsModelConfig {
                model: Some(onnx_path.to_string_lossy().into_owned()),
                tokens: Some(tokens_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
            num_threads: 1,
            ..Default::default()
        };
        let config = OfflineTtsConfig {
            model,
            ..Default::default()
        };
        let tts = OfflineTts::create(&config).ok_or_else(|| TtsError::InvalidResponse {
            detail: format!(
                "không khởi tạo được Piper TTS cho giọng '{}' (kiểm tra file model)",
                voice.label
            ),
        })?;
        Ok(Self {
            voice_id: voice.id,
            tts: Arc::new(Mutex::new(tts)),
        })
    }

    /// Id of the loaded voice (e.g. `"ngochuyen"`).
    pub fn voice_id(&self) -> &'static str {
        self.voice_id
    }

    /// Blocking synthesis: phonemize, then generate. Runs on a blocking
    /// thread (phonemize extracts espeak-ng-data on first use; generation
    /// is CPU-bound ONNX inference).
    fn synthesize_blocking(
        tts: &Arc<Mutex<OfflineTts>>,
        text: &str,
        speed: f32,
    ) -> Result<TtsSynthesis, TtsError> {
        let phonemized = piper_phonemize::phonemize(text, ESPEAK_VOICE_VI).ok_or_else(|| {
            TtsError::InvalidResponse {
                detail: "không tách được âm tiết tiếng Việt (piper-phonemize)".to_string(),
            }
        })?;
        let mut codepoints: Vec<Vec<i32>> = Vec::new();
        for sentence in 0..phonemized.num_sentences() {
            let phonemes =
                phonemized
                    .get_phonemes(sentence)
                    .ok_or_else(|| TtsError::InvalidResponse {
                        detail: "không đọc được âm tiết của một câu (piper-phonemize)".to_string(),
                    })?;
            if !phonemes.is_empty() {
                codepoints.push(phonemes);
            }
        }
        if codepoints.is_empty() {
            return Err(TtsError::InvalidResponse {
                detail: "không có âm tiết nào để đọc".to_string(),
            });
        }
        // `silence_scale: 1.0` matches the Python reference run (the
        // sherpa-onnx Rust crate defaults it to 0.2); it only scales
        // inter-sentence silence and is a no-op for single sentences.
        let generation = GenerationConfig {
            sid: 0,
            speed,
            silence_scale: 1.0,
            phoneme_codepoints: Some(codepoints),
            ..Default::default()
        };
        let guard = tts.lock().unwrap_or_else(|err| err.into_inner());
        let audio = guard
            .generate_with_config("", &generation, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| TtsError::InvalidResponse {
                detail: "Piper không tạo được âm thanh".to_string(),
            })?;
        let sample_rate = audio.sample_rate();
        if sample_rate <= 0 {
            return Err(TtsError::InvalidResponse {
                detail: "Piper trả về sample rate không hợp lệ".to_string(),
            });
        }
        let wav = encode_wav_pcm16(audio.samples(), sample_rate as u32);
        Ok(TtsSynthesis {
            audio: wav,
            format: AudioFormat::Wav,
            retries: Vec::new(),
        })
    }
}

impl TtsEngine for PiperTtsEngine {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<TtsSynthesis, TtsError>> {
        let text = text.to_string();
        let voice = voice.to_string();
        let rate = rate.to_string();
        let tts = Arc::clone(&self.tts);
        let voice_id = self.voice_id;
        Box::pin(async move {
            if text.trim().is_empty() {
                return Err(TtsError::EmptyInput);
            }
            if voice != voice_id {
                return Err(TtsError::EngineUnavailable {
                    detail: format!(
                        "giọng '{voice}' không phải giọng Piper đã tải ('{voice_id}'); chọn giọng trong Cài đặt → Giọng đọc"
                    ),
                });
            }
            let speed = parse_rate_to_speed(&rate);
            match tokio::task::spawn_blocking(move || Self::synthesize_blocking(&tts, &text, speed))
                .await
            {
                Ok(result) => result,
                Err(join_err) => Err(TtsError::InvalidResponse {
                    detail: format!("luồng tổng hợp Piper dừng bất ngờ: {join_err}"),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        encode_wav_pcm16, find_piper_voice, parse_rate_to_speed, voice_dir, voice_files_present,
        PIPER_VOICES,
    };
    use std::path::PathBuf;

    #[test]
    fn piper_voice_registry_has_ngoc_huyen_first_with_exact_size() {
        assert!(!PIPER_VOICES.is_empty());
        let first = PIPER_VOICES[0];
        assert_eq!(first.id, "ngochuyen");
        assert_eq!(first.onnx_file, "ngochuyen.onnx");
        assert_eq!(first.onnx_bytes, 63_516_173);
        assert_eq!(find_piper_voice("ngochuyen"), Some(&first));
        assert_eq!(find_piper_voice("unknown"), None);
    }

    #[test]
    fn parse_rate_to_speed_maps_edge_rates_and_falls_back_to_normal() {
        assert!((parse_rate_to_speed("+0%") - 1.0).abs() < f32::EPSILON);
        assert!((parse_rate_to_speed("+10%") - 1.1).abs() < 1e-6);
        assert!((parse_rate_to_speed("-20%") - 0.8).abs() < 1e-6);
        assert!((parse_rate_to_speed("+20%") - 1.2).abs() < 1e-6);
        assert!((parse_rate_to_speed("  -10%  ") - 0.9).abs() < 1e-6);
        assert!((parse_rate_to_speed("fast") - 1.0).abs() < f32::EPSILON);
        assert!((parse_rate_to_speed("") - 1.0).abs() < f32::EPSILON);
        // Clamped, never extreme.
        assert!((parse_rate_to_speed("+200%") - 2.0).abs() < f32::EPSILON);
        assert!((parse_rate_to_speed("-90%") - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn encode_wav_pcm16_writes_valid_riff_header_and_clamps_samples() {
        let wav = encode_wav_pcm16(&[0.0, 0.5, -0.5, 1.0, -1.0, 2.0], 22_050);
        assert_eq!(wav.len(), 44 + 12);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 22_050);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // mono
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16); // bits
        let samples: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, vec![0, 16383, -16383, 32767, -32767, 32767]);
    }

    #[test]
    fn voice_files_present_requires_both_onnx_and_tokens() {
        let dir = std::env::temp_dir().join(format!(
            "cap2voice_piper_present_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let voice = &PIPER_VOICES[0];
        assert!(!voice_files_present(voice, &dir));
        let voice_path = voice_dir(&dir, voice.id);
        std::fs::create_dir_all(&voice_path).expect("create voice dir");
        std::fs::write(voice_path.join(voice.onnx_file), b"fake-onnx").expect("write onnx");
        assert!(!voice_files_present(voice, &dir));
        std::fs::write(voice_path.join(super::PIPER_TOKENS_FILE), b"_ 0\n").expect("write tokens");
        assert!(voice_files_present(voice, &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_voices_dir_respects_env_override() {
        let custom = PathBuf::from(r"C:\Custom\cap2voice\voices\piper");
        std::env::set_var("CAP2VOICE_PIPER_VOICES_DIR", &custom);
        let dir = super::default_voices_dir().expect("env override dir");
        assert_eq!(dir, custom);
        std::env::remove_var("CAP2VOICE_PIPER_VOICES_DIR");
    }
}
