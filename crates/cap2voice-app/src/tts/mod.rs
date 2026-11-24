//! Native TTS engines: Edge-TTS over WebSocket (default) and Piper offline
//! VITS (selectable), behind one switchable `TtsEngine` implementation.
//!
//! The pipeline holds an `Arc<SwitchableTtsEngine>`; switching engines in
//! Settings flips an internal lock, so no pipeline restart is needed. The
//! Piper model loads lazily in a background thread on first use (or when
//! the engine is selected in Settings) and never blocks startup; synthesis
//! requested before the model is ready fails with a clear
//! `TtsError::EngineUnavailable` telling the user to download the voice in
//! Settings → Giọng đọc. The Chinese click-to-replay always uses Edge-TTS
//! (`zh-CN-XiaoxiaoNeural`) via `SwitchableTtsEngine::edge_engine`, never
//! Piper — Piper only ships Vietnamese voices.

pub mod edge_tts;
pub mod piper;

pub use edge_tts::{EdgeTtsEngine, AVAILABLE_VOICES};
pub use piper::{
    default_voices_dir, download_voice, find_piper_voice, piper_onnx_url, piper_tokens_url,
    voice_dir, voice_files_present, PiperDownloadEvent, PiperTtsEngine, PiperVoice,
    DEFAULT_PIPER_VOICE_ID, PIPER_TOKENS_FILE, PIPER_VOICES,
};

use crate::api::{BoxFuture, TtsEngine, TtsError, TtsSynthesis};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

/// User-selectable TTS engine (Settings → Giọng đọc tab,
/// `voice.engine` in `config.toml`). Mirrors `translate::TranslationEngine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TtsEngineKind {
    /// Microsoft Edge-TTS over WebSocket (online, default).
    #[default]
    Edge,
    /// Piper offline VITS (downloaded on demand, no network at synthesis).
    Piper,
}

impl TtsEngineKind {
    /// All selectable engines, in Settings order.
    pub const ALL: [Self; 2] = [Self::Edge, Self::Piper];

    /// Stable lowercase id used in `config.toml` and cache keys.
    pub fn as_str(self) -> &'static str {
        match self {
            TtsEngineKind::Edge => "edge",
            TtsEngineKind::Piper => "piper",
        }
    }

    /// Vietnamese UI label.
    pub fn label(self) -> &'static str {
        match self {
            TtsEngineKind::Edge => "Edge-TTS (online)",
            TtsEngineKind::Piper => "Piper (offline)",
        }
    }
}

/// Lifecycle of the lazily loaded Piper model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiperState {
    /// Never requested yet.
    NotLoaded,
    /// Background load in progress.
    Loading,
    /// Ready to synthesize.
    Ready,
    /// Load failed, or the voice has not been downloaded yet; the reason
    /// is shown in the UI.
    Failed(String),
}

/// Observable TTS status for the Settings window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsStatus {
    pub engine: TtsEngineKind,
    pub piper: PiperState,
    /// Id of the Piper voice the switchable is configured to use
    /// (`config.voice.voice` when the engine is Piper).
    pub piper_voice_id: String,
}

/// A `TtsEngine` that delegates to the active engine (Edge-TTS or Piper),
/// loading the Piper model lazily in the background. The `voice` argument
/// of `synthesize` is engine-dependent (Edge voice id vs Piper voice id);
/// the caller passes the voice that matches the active engine.
pub struct SwitchableTtsEngine {
    engine: RwLock<TtsEngineKind>,
    edge: Arc<EdgeTtsEngine>,
    piper: RwLock<Option<Arc<PiperTtsEngine>>>,
    piper_state: RwLock<PiperState>,
    piper_voice_id: RwLock<String>,
    voices_dir: PathBuf,
    load_started: Mutex<bool>,
}

impl SwitchableTtsEngine {
    /// Build the switchable engine. The Edge client is stateless and always
    /// available; the Piper model is NOT loaded here — it loads in the
    /// background on first use (or when the engine is switched to Piper).
    pub fn new(engine: TtsEngineKind, piper_voice_id: String, voices_dir: PathBuf) -> Arc<Self> {
        let switchable = Arc::new(Self {
            engine: RwLock::new(engine),
            edge: Arc::new(EdgeTtsEngine::new()),
            piper: RwLock::new(None),
            piper_state: RwLock::new(PiperState::NotLoaded),
            piper_voice_id: RwLock::new(piper_voice_id),
            voices_dir,
            load_started: Mutex::new(false),
        });
        if engine == TtsEngineKind::Piper {
            switchable.ensure_piper_loading();
        }
        switchable
    }

    /// Current engine (what the next synthesis will use).
    pub fn current_engine(&self) -> TtsEngineKind {
        *self.engine.read().unwrap_or_else(|err| err.into_inner())
    }

    /// Switch the active engine at runtime (no restart). Switching to
    /// Piper triggers the background model load when needed.
    pub fn set_engine(self: &Arc<Self>, engine: TtsEngineKind) {
        *self.engine.write().unwrap_or_else(|err| err.into_inner()) = engine;
        if engine == TtsEngineKind::Piper {
            self.ensure_piper_loading();
        }
    }

    /// Update the Piper voice id the switchable will load (called when the
    /// voice changes in Settings while Piper is the active engine). The
    /// next Piper synthesis uses the new voice; a running load for the old
    /// voice is left alone and its result is dropped.
    pub fn set_piper_voice(self: &Arc<Self>, voice_id: String) {
        let changed = {
            let mut slot = self
                .piper_voice_id
                .write()
                .unwrap_or_else(|err| err.into_inner());
            if *slot == voice_id {
                false
            } else {
                *slot = voice_id;
                true
            }
        };
        if changed {
            *self.piper.write().unwrap_or_else(|err| err.into_inner()) = None;
            *self
                .piper_state
                .write()
                .unwrap_or_else(|err| err.into_inner()) = PiperState::NotLoaded;
            *self
                .load_started
                .lock()
                .unwrap_or_else(|err| err.into_inner()) = false;
            if self.current_engine() == TtsEngineKind::Piper {
                self.ensure_piper_loading();
            }
        }
    }

    /// The inner Edge-TTS client, for the Chinese click-to-replay (which
    /// always uses Edge-TTS, never Piper).
    pub fn edge_engine(&self) -> Arc<EdgeTtsEngine> {
        Arc::clone(&self.edge)
    }

    /// Observable status for the Settings window.
    pub fn status(&self) -> TtsStatus {
        TtsStatus {
            engine: self.current_engine(),
            piper: self
                .piper_state
                .read()
                .unwrap_or_else(|err| err.into_inner())
                .clone(),
            piper_voice_id: self
                .piper_voice_id
                .read()
                .unwrap_or_else(|err| err.into_inner())
                .clone(),
        }
    }

    /// Start the Piper background load once (idempotent). The load runs on
    /// a plain thread so it never blocks the Tokio runtime or the UI.
    pub fn ensure_piper_loading(self: &Arc<Self>) {
        {
            let mut started = self
                .load_started
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if *started {
                return;
            }
            *started = true;
        }
        {
            let mut state = self
                .piper_state
                .write()
                .unwrap_or_else(|err| err.into_inner());
            if *state == PiperState::Ready {
                return;
            }
            *state = PiperState::Loading;
        }
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("cap2voice-piper-load".to_string())
            .spawn(move || {
                let voice_id = this
                    .piper_voice_id
                    .read()
                    .unwrap_or_else(|err| err.into_inner())
                    .clone();
                let result = match find_piper_voice(&voice_id) {
                    Some(voice) => PiperTtsEngine::load(voice, &this.voices_dir),
                    None => Err(TtsError::EngineUnavailable {
                        detail: format!("giọng Piper '{voice_id}' không có trong danh sách"),
                    }),
                };
                match result {
                    Ok(engine) => {
                        *this.piper.write().unwrap_or_else(|err| err.into_inner()) =
                            Some(Arc::new(engine));
                        *this
                            .piper_state
                            .write()
                            .unwrap_or_else(|err| err.into_inner()) = PiperState::Ready;
                    }
                    Err(err) => {
                        *this
                            .piper_state
                            .write()
                            .unwrap_or_else(|err| err.into_inner()) =
                            PiperState::Failed(err.to_string());
                    }
                }
            });
        if spawned.is_err() {
            *self
                .piper_state
                .write()
                .unwrap_or_else(|err| err.into_inner()) =
                PiperState::Failed("không tạo được luồng tải model Piper".to_string());
        }
    }

    /// Synthesize with an explicit engine, without changing the current
    /// one. Used by the Settings voice preview, which previews the DRAFT
    /// engine/voice before the user saves.
    pub fn synthesize_as<'a>(
        &'a self,
        engine: TtsEngineKind,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<TtsSynthesis, TtsError>> {
        match engine {
            TtsEngineKind::Edge => self.edge.synthesize(text, voice, rate),
            TtsEngineKind::Piper => {
                // Clone the `Arc` out of the lock: the future returned by
                // `synthesize` borrows its engine, so it must own the
                // `Arc` (a lock guard is a local and cannot be borrowed
                // across the returned future).
                let piper = self
                    .piper
                    .read()
                    .unwrap_or_else(|err| err.into_inner())
                    .as_ref()
                    .cloned();
                match piper {
                    Some(engine) => {
                        Box::pin(async move { engine.synthesize(text, voice, rate).await })
                    }
                    None => {
                        let voice_id = self
                            .piper_voice_id
                            .read()
                            .unwrap_or_else(|err| err.into_inner())
                            .clone();
                        let detail = match self
                            .piper_state
                            .read()
                            .unwrap_or_else(|err| err.into_inner())
                            .clone()
                        {
                            PiperState::Failed(reason) => reason,
                            PiperState::Loading | PiperState::NotLoaded => format!(
                                "giọng Piper '{voice_id}' chưa sẵn sàng — bấm 'Tải giọng' trong Cài đặt → Giọng đọc"
                            ),
                            PiperState::Ready => {
                                "model Piper đã sẵn sàng nhưng engine chưa tải — thử lại sau giây lát"
                                    .to_string()
                            }
                        };
                        Box::pin(async move { Err(TtsError::EngineUnavailable { detail }) })
                    }
                }
            }
        }
    }
}

impl TtsEngine for SwitchableTtsEngine {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<TtsSynthesis, TtsError>> {
        match self.current_engine() {
            TtsEngineKind::Edge => self.edge.synthesize(text, voice, rate),
            TtsEngineKind::Piper => {
                // Clone the `Arc` out of the lock (see `synthesize_as`):
                // the returned future must own its engine.
                let piper = self
                    .piper
                    .read()
                    .unwrap_or_else(|err| err.into_inner())
                    .as_ref()
                    .cloned();
                match piper {
                    Some(engine) => {
                        Box::pin(async move { engine.synthesize(text, voice, rate).await })
                    }
                    None => {
                        // Model not ready yet: the load is triggered on
                        // engine switch (`set_engine`) and at construction
                        // when Piper is the configured engine, so reaching
                        // here means it is still in flight or failed —
                        // report the state instead of blocking the
                        // pipeline on this sentence.
                        let detail = match self
                            .piper_state
                            .read()
                            .unwrap_or_else(|err| err.into_inner())
                            .clone()
                        {
                            PiperState::Failed(reason) => reason,
                            _ => {
                                let voice_id = self
                                    .piper_voice_id
                                    .read()
                                    .unwrap_or_else(|err| err.into_inner())
                                    .clone();
                                format!(
                                    "giọng Piper '{voice_id}' chưa sẵn sàng — bấm 'Tải giọng' trong Cài đặt → Giọng đọc"
                                )
                            }
                        };
                        Box::pin(async move { Err(TtsError::EngineUnavailable { detail }) })
                    }
                }
            }
        }
    }
}
