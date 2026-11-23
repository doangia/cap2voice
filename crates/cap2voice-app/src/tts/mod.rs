//! Native TTS engines (D1): Edge-TTS over WebSocket, no Python sidecar.

pub mod edge_tts;

pub use edge_tts::EdgeTtsEngine;
