//! Translation engines: Helsinki-NLP ONNX model (default) and the
//! Vietphrase dictionary translator, behind one switchable `Translator`
//! implementation.
//!
//! The pipeline holds an `Arc<SwitchableTranslator>`; switching engines in
//! Settings flips an internal lock, so no pipeline restart is needed.
//! The Helsinki model loads in a background thread on first use (or at
//! app startup when it is the configured engine) and never blocks startup;
//! translations requested before the model is ready fall back to
//! Vietphrase for that sentence and the fallback is counted and reported
//! via [`SwitchableTranslator::status`] (the overlay shows
//! "Đang tải model dịch…" while loading).

pub mod helsinki;
pub mod rule_engine;
pub mod vietphrase;

pub use helsinki::{default_model_dir, HelsinkiError, HelsinkiTranslator};
pub use vietphrase::{
    default_dict_dir, load_default_dictionaries, load_dictionary_set, DictionarySet,
    VietPhraseTranslator,
};

use crate::api::{BoxFuture, TranslateError, Translator};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// User-selectable translation engine (Settings → Dịch tab,
/// `translation.engine` in `config.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranslationEngine {
    /// Helsinki-NLP `opus-mt-zh-vi` ONNX model (better overall quality).
    #[default]
    Helsinki,
    /// Vietphrase dictionary translator (offline, correct on proper names).
    Vietphrase,
}

impl TranslationEngine {
    /// All selectable engines, in Settings order.
    pub const ALL: [Self; 2] = [Self::Helsinki, Self::Vietphrase];

    /// Stable lowercase id used in `config.toml` and cache keys.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Helsinki => "helsinki",
            Self::Vietphrase => "vietphrase",
        }
    }

    /// Vietnamese UI label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Helsinki => "Helsinki (mô hình AI)",
            Self::Vietphrase => "Vietphrase (từ điển)",
        }
    }
}

/// Lifecycle of the lazily loaded Helsinki model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelsinkiState {
    /// Never requested yet.
    NotLoaded,
    /// Background load in progress.
    Loading,
    /// Ready to translate.
    Ready,
    /// Load failed; the reason is shown in the UI.
    Failed(String),
}

/// Observable translator status for the overlay / Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatorStatus {
    pub engine: TranslationEngine,
    pub helsinki: HelsinkiState,
    /// Whether the Vietphrase dictionaries loaded (the fallback engine).
    pub vietphrase_available: bool,
    /// Sentences translated by Vietphrase because Helsinki was not ready.
    pub helsinki_fallbacks: u64,
}

/// A `Translator` that delegates to the active engine (Helsinki or
/// Vietphrase), loading the Helsinki model lazily in the background.
pub struct SwitchableTranslator {
    engine: RwLock<TranslationEngine>,
    vietphrase: Option<Arc<VietPhraseTranslator>>,
    helsinki: RwLock<Option<Arc<Mutex<HelsinkiTranslator>>>>,
    helsinki_state: RwLock<HelsinkiState>,
    model_dir: PathBuf,
    load_started: Mutex<bool>,
    fallbacks: AtomicU64,
}

impl SwitchableTranslator {
    /// Build the translator. `vietphrase_dicts` is `None` when the
    /// dictionaries are missing (Vietphrase unavailable); the Helsinki
    /// model is NOT loaded here — it loads in the background on first use.
    /// Returns `Err` only when BOTH engines are unavailable. The result
    /// is an `Arc` because the background load thread holds one.
    pub fn new(
        vietphrase_dicts: Option<DictionarySet>,
        model_dir: PathBuf,
        engine: TranslationEngine,
    ) -> Result<Arc<Self>, String> {
        let vietphrase =
            vietphrase_dicts.map(|dicts| Arc::new(VietPhraseTranslator::new(dicts, None)));
        if vietphrase.is_none() && !model_dir.join("encoder_model.onnx").exists() {
            return Err(format!(
                "no translation engine available: Vietphrase dictionaries are missing and no Helsinki model was found at {}",
                model_dir.display()
            ));
        }
        let translator = Arc::new(Self {
            engine: RwLock::new(engine),
            vietphrase,
            helsinki: RwLock::new(None),
            helsinki_state: RwLock::new(HelsinkiState::NotLoaded),
            model_dir,
            load_started: Mutex::new(false),
            fallbacks: AtomicU64::new(0),
        });
        if engine == TranslationEngine::Helsinki {
            translator.ensure_helsinki_loading();
        }
        Ok(translator)
    }

    /// Current engine (what the next translation will use, unless Helsinki
    /// is still loading and falls back to Vietphrase).
    pub fn current_engine(&self) -> TranslationEngine {
        *self.engine.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Switch the active engine at runtime (no restart). Switching to
    /// Helsinki triggers the background model load when needed.
    pub fn set_engine(self: &Arc<Self>, engine: TranslationEngine) {
        *self.engine.write().unwrap_or_else(|e| e.into_inner()) = engine;
        if engine == TranslationEngine::Helsinki {
            self.ensure_helsinki_loading();
        }
    }

    /// Observable status for the overlay / Settings.
    pub fn status(&self) -> TranslatorStatus {
        TranslatorStatus {
            engine: self.current_engine(),
            helsinki: self
                .helsinki_state
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vietphrase_available: self.vietphrase.is_some(),
            helsinki_fallbacks: self.fallbacks.load(Ordering::Relaxed),
        }
    }

    /// Start the Helsinki background load once (idempotent). The load runs
    /// on a plain thread so it never blocks the Tokio runtime or the UI.
    pub fn ensure_helsinki_loading(self: &Arc<Self>) {
        {
            let mut started = self.load_started.lock().unwrap_or_else(|e| e.into_inner());
            if *started {
                return;
            }
            *started = true;
        }
        {
            let mut state = self
                .helsinki_state
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if *state == HelsinkiState::Ready {
                return;
            }
            *state = HelsinkiState::Loading;
        }
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("helsinki-model-load".to_string())
            .spawn(move || match HelsinkiTranslator::load(&this.model_dir) {
                Ok(translator) => {
                    *this.helsinki.write().unwrap_or_else(|e| e.into_inner()) =
                        Some(Arc::new(Mutex::new(translator)));
                    *this
                        .helsinki_state
                        .write()
                        .unwrap_or_else(|e| e.into_inner()) = HelsinkiState::Ready;
                }
                Err(err) => {
                    *this
                        .helsinki_state
                        .write()
                        .unwrap_or_else(|e| e.into_inner()) =
                        HelsinkiState::Failed(err.to_string());
                }
            });
        if spawned.is_err() {
            *self
                .helsinki_state
                .write()
                .unwrap_or_else(|e| e.into_inner()) =
                HelsinkiState::Failed("không tạo được luồng tải model Helsinki".to_string());
        }
    }

    fn translate_vietphrase(&self, chinese: &str) -> Result<String, TranslateError> {
        match &self.vietphrase {
            Some(translator) => Ok(translator.translate(chinese)),
            None => Err(TranslateError::EngineUnavailable {
                detail: "từ điển Vietphrase chưa có — hãy chép 5 file TXT vào thư mục dữ liệu (xem README)".to_string(),
            }),
        }
    }

    fn map_helsinki_error(err: HelsinkiError) -> TranslateError {
        match err {
            HelsinkiError::EmptyInput => TranslateError::EmptyInput,
            HelsinkiError::ModelDirMissing { detail, .. }
            | HelsinkiError::TokenizerLoad { detail }
            | HelsinkiError::SessionLoad { detail } => TranslateError::Upstream {
                status: 503,
                detail,
            },
            HelsinkiError::Inference { detail } => TranslateError::Upstream {
                status: 500,
                detail,
            },
        }
    }
}

impl Translator for SwitchableTranslator {
    fn engine_id(&self) -> Option<&'static str> {
        Some(self.current_engine().as_str())
    }

    fn translate<'a>(&'a self, chinese: &'a str) -> BoxFuture<'a, Result<String, TranslateError>> {
        let chinese = chinese.to_string();
        Box::pin(async move {
            if chinese.trim().is_empty() {
                return Err(TranslateError::EmptyInput);
            }
            match self.current_engine() {
                TranslationEngine::Vietphrase => self.translate_vietphrase(&chinese),
                TranslationEngine::Helsinki => {
                    let helsinki = self
                        .helsinki
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    match helsinki {
                        Some(translator) => {
                            let text = chinese.clone();
                            let res = tokio::task::spawn_blocking(move || {
                                let mut guard =
                                    translator.lock().unwrap_or_else(|e| e.into_inner());
                                guard.translate_blocking(&text)
                            })
                            .await;
                            match res {
                                Ok(Ok(vietnamese)) => Ok(vietnamese),
                                Ok(Err(err)) => Err(Self::map_helsinki_error(err)),
                                Err(join_err) => Err(TranslateError::Upstream {
                                    status: 500,
                                    detail: format!("luồng dịch Helsinki dừng bất ngờ: {join_err}"),
                                }),
                            }
                        }
                        None => {
                            // Model not ready yet: fall back to Vietphrase
                            // for this sentence (counted + visible in the
                            // status), instead of blocking the pipeline.
                            // The load is always triggered before any
                            // Helsinki translate can happen (at
                            // construction when Helsinki is the configured
                            // engine, or on `set_engine(Helsinki)`), so
                            // reaching here means it is still in flight
                            // (or failed — see `status()`).
                            self.fallbacks.fetch_add(1, Ordering::Relaxed);
                            match self.translate_vietphrase(&chinese) {
                                Ok(vietnamese) => Ok(vietnamese),
                                Err(_) => Err(TranslateError::EngineUnavailable {
                                    detail: "model Helsinki đang tải, thử lại sau giây lát"
                                        .to_string(),
                                }),
                            }
                        }
                    }
                }
            }
        })
    }
}
