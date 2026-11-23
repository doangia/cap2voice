//! Helsinki-NLP `opus-mt-zh-vi` translator via ONNX Runtime (`ort`).
//!
//! Design (proven by the standalone spike on the real PC, 2026-11-22):
//! the PyTorch model is exported once to ONNX (`encoder_model.onnx` +
//! `decoder_model.onnx` + `tokenizer.json`, in the model dir) and run
//! with `ort` 2.x. Decoding is greedy, **without** a KV cache: the plain
//! decoder is re-run each step with the full generated prefix. The
//! KV-cache path (`decoder_with_past_model.onnx`) was abandoned after it
//! produced wrong tokens from step 2 in `ort` 2.0.0-rc.15 despite
//! verified-identical values, names and shapes versus the working Python
//! run; the no-past path is simpler, provably correct (identical outputs
//! to the Python greedy run on all 8 spike lines), and fast enough
//! (27–140 ms per sentence on the PC, inside the 2.0 s realtime gate).
//!
//! Only the encoder + decoder ONNX files and `tokenizer.json` are needed;
//! `decoder_with_past_model.onnx` is NOT used. The model files live
//! outside the repo (see [`default_model_dir`]).

use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Decoder start token (`<pad>`, 65000) and EOS (`</s>`, 0) from the
/// model's `generation_config.json`.
const DECODER_START_TOKEN_ID: i64 = 65000;
const EOS_TOKEN_ID: i64 = 0;
/// `<pad>` (65000) is banned as a generated token (Python `bad_words_ids`).
const BANNED_TOKEN_ID: usize = 65000;
const VOCAB_SIZE: usize = 65001;
/// Maximum generated tokens per sentence (Python `max_new_tokens` guard).
const MAX_NEW_TOKENS: usize = 64;

/// Default model directory: `%APPDATA%\cap2voice\models\opus-mt-zh-vi`.
/// Override with `CAP2VOICE_HELSINKI_MODEL_DIR` (tests, power users).
pub fn default_model_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("CAP2VOICE_HELSINKI_MODEL_DIR") {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Err("APPDATA environment variable is not set".to_string());
    };
    Ok(PathBuf::from(appdata)
        .join("cap2voice")
        .join("models")
        .join("opus-mt-zh-vi"))
}

/// Typed failures loading or running the Helsinki model.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum HelsinkiError {
    #[error("không tìm thấy model Helsinki tại {dir}: {detail}")]
    ModelDirMissing { dir: PathBuf, detail: String },

    #[error("không tải được tokenizer Helsinki: {detail}")]
    TokenizerLoad { detail: String },

    #[error("không tải được model ONNX Helsinki: {detail}")]
    SessionLoad { detail: String },

    #[error("lỗi suy luận Helsinki: {detail}")]
    Inference { detail: String },

    #[error("chuỗi cần dịch không được để trống")]
    EmptyInput,
}

fn tensor_i64(shape: Vec<i64>, data: Vec<i64>) -> Result<ort::value::Value, HelsinkiError> {
    Tensor::from_array((shape, data.into_boxed_slice()))
        .map(|t| t.into_dyn())
        .map_err(|err| HelsinkiError::Inference {
            detail: err.to_string(),
        })
}

fn tensor_f32(shape: Vec<i64>, data: Vec<f32>) -> Result<ort::value::Value, HelsinkiError> {
    Tensor::from_array((shape, data.into_boxed_slice()))
        .map(|t| t.into_dyn())
        .map_err(|err| HelsinkiError::Inference {
            detail: err.to_string(),
        })
}

/// Argmax over the LAST position of logits with shape `[1, seq, VOCAB]`,
/// skipping the banned `<pad>` token.
fn argmax_last(shape: &[i64], data: &[f32]) -> i64 {
    let seq = shape[1] as usize;
    let start = (seq - 1) * VOCAB_SIZE;
    let mut best = 0usize;
    let mut best_v = f32::NEG_INFINITY;
    for (i, &v) in data[start..start + VOCAB_SIZE].iter().enumerate() {
        if i == BANNED_TOKEN_ID {
            continue;
        }
        if v > best_v {
            best_v = v;
            best = i;
        }
    }
    best as i64
}

/// Loaded Helsinki model: tokenizer + encoder/decoder ONNX sessions.
/// `ort::Session::run` takes `&mut self`, so the struct is used behind a
/// `Mutex` and inference runs on a blocking thread (`spawn_blocking`).
pub struct HelsinkiTranslator {
    tokenizer: tokenizers::Tokenizer,
    encoder: Session,
    decoder: Session,
}

impl HelsinkiTranslator {
    /// Load the model from `dir` (expects `encoder_model.onnx`,
    /// `decoder_model.onnx`, `tokenizer.json`). Slow (seconds, ~580 MB
    /// of weights) — call from a background thread, never on the UI thread.
    pub fn load(dir: &Path) -> Result<Self, HelsinkiError> {
        let missing: Vec<&str> = ["encoder_model.onnx", "decoder_model.onnx", "tokenizer.json"]
            .into_iter()
            .filter(|name| !dir.join(name).is_file())
            .collect();
        if !missing.is_empty() {
            return Err(HelsinkiError::ModelDirMissing {
                dir: dir.to_path_buf(),
                detail: format!(
                    "missing {} — copy encoder_model.onnx, decoder_model.onnx and tokenizer.json from the opus-mt-zh-vi ONNX export into this directory (see README)",
                    missing.join(", ")
                ),
            });
        }
        let tokenizer =
            tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|err| {
                HelsinkiError::TokenizerLoad {
                    detail: err.to_string(),
                }
            })?;
        // The `Environment` is only needed while building the sessions:
        // each `Session` keeps its own environment alive internally
        // (`SharedSessionInner` holds it), so it is not stored here.
        let env = ort::init()
            .build()
            .map_err(|err| HelsinkiError::SessionLoad {
                detail: err.to_string(),
            })?;
        let encoder = Session::builder(&env)
            .map_err(|err| HelsinkiError::SessionLoad {
                detail: err.to_string(),
            })?
            .commit_from_file(dir.join("encoder_model.onnx"))
            .map_err(|err| HelsinkiError::SessionLoad {
                detail: err.to_string(),
            })?;
        let decoder = Session::builder(&env)
            .map_err(|err| HelsinkiError::SessionLoad {
                detail: err.to_string(),
            })?
            .commit_from_file(dir.join("decoder_model.onnx"))
            .map_err(|err| HelsinkiError::SessionLoad {
                detail: err.to_string(),
            })?;
        Ok(Self {
            tokenizer,
            encoder,
            decoder,
        })
    }

    /// Translate one Chinese sentence to Vietnamese (greedy, no KV cache).
    /// Blocking CPU work (~30–140 ms on the PC) — call via `spawn_blocking`.
    pub fn translate_blocking(&mut self, chinese: &str) -> Result<String, HelsinkiError> {
        if chinese.trim().is_empty() {
            return Err(HelsinkiError::EmptyInput);
        }
        // `add_special_tokens=true`: Marian appends `</s>` (id 0) to the
        // source, matching the Python benchmark exactly.
        let ids: Vec<i64> = self
            .tokenizer
            .encode(chinese, true)
            .map_err(|err| HelsinkiError::Inference {
                detail: err.to_string(),
            })?
            .get_ids()
            .iter()
            .map(|id| *id as i64)
            .collect();
        let n = ids.len() as i64;

        let enc_out = self
            .encoder
            .run(ort::inputs![
                tensor_i64(vec![1, n], ids)?,
                tensor_i64(vec![1, n], vec![1i64; n as usize])?,
            ])
            .map_err(|err| HelsinkiError::Inference {
                detail: err.to_string(),
            })?;
        let (enc_shape, enc_data) = {
            let (shape, data) = enc_out["last_hidden_state"]
                .try_extract_tensor::<f32>()
                .map_err(|err| HelsinkiError::Inference {
                    detail: err.to_string(),
                })?;
            (shape.iter().copied().collect::<Vec<i64>>(), data.to_vec())
        };

        let mut gen: Vec<i64> = vec![DECODER_START_TOKEN_ID];
        let mut out_ids: Vec<i64> = Vec::new();
        for _ in 0..MAX_NEW_TOKENS {
            let m = gen.len() as i64;
            let out = self
                .decoder
                .run(ort::inputs![
                    "input_ids" => tensor_i64(vec![1, m], gen.clone())?,
                    "encoder_hidden_states" => tensor_f32(enc_shape.clone(), enc_data.clone())?,
                    "encoder_attention_mask" => tensor_i64(vec![1, n], vec![1i64; n as usize])?,
                ])
                .map_err(|err| HelsinkiError::Inference {
                    detail: err.to_string(),
                })?;
            let (shape, data) = out["logits"].try_extract_tensor::<f32>().map_err(|err| {
                HelsinkiError::Inference {
                    detail: err.to_string(),
                }
            })?;
            let shape: Vec<i64> = shape.iter().copied().collect();
            let next_id = argmax_last(&shape, data);
            if next_id == EOS_TOKEN_ID {
                break;
            }
            out_ids.push(next_id);
            gen.push(next_id);
        }
        self.tokenizer
            .decode(
                &out_ids.iter().map(|id| *id as u32).collect::<Vec<_>>(),
                true,
            )
            .map_err(|err| HelsinkiError::Inference {
                detail: err.to_string(),
            })
    }
}
