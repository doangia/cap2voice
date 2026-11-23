//! Native Rust Edge-TTS client (D1): speaks the Microsoft Edge online TTS
//! service directly over WebSocket, with no Python sidecar and no
//! `edge-tts` crate (none exists — the protocol is implemented here from
//! the installed `edge-tts` Python source, `edge_tts` 7.x, Chromium 140).
//!
//! Protocol summary (verified against the installed source on the PC):
//! - `wss://api.msedgeservices.com/tts/cognitiveservices/websocket/v1`
//!   with `Ocp-Apim-Subscription-Key=6A5AA1D4EAFF4E9FB37E23D68491D6F4`,
//!   `ConnectionId={uuid, no dashes}`, `Sec-MS-GEC={token}` and
//!   `Sec-MS-GEC-Version=1-140.0.3485.14`.
//! - `Sec-MS-GEC` = uppercase SHA-256 hex of
//!   `"{winfiletime_ticks}{TRUSTED_CLIENT_TOKEN}"`, ticks = unix time +
//!   11644473600, rounded down to the nearest 300 s, in 100-ns units.
//! - Headers: `Pragma: no-cache`, `Cache-Control: no-cache`,
//!   `Origin: chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold`,
//!   `Sec-WebSocket-Protocol: synthesize`, `Sec-WebSocket-Version: 13`,
//!   Chrome/Edge 140 User-Agent, `Accept-Encoding: gzip, deflate, br`,
//!   `Accept-Language: en-US,en;q=0.9`. The `edge-tts` Python client offers
//!   permessage-deflate (`compress=15`), but tungstenite 0.26 removed
//!   deflate support, so this client does not offer it; the service
//!   accepts plain (uncompressed) frames (verified live — see the
//!   integration test).
//!   The Python client also sends `Sec-WebSocket-Protocol: synthesize`,
//!   but this client does NOT: tungstenite validates the subprotocol echo
//!   and the Edge server never echoes it, so the handshake would fail with
//!   "Server sent no subprotocol". The server does not require the header
//!   (verified live: 13,417 audio bytes for "Xin chào" with it removed).
//! - Client sends `speech.config` (JSON, `audio-24khz-48kbitrate-mono-mp3`,
//!   sentence boundaries on) then one SSML message per text chunk
//!   (≤4096 bytes, split on safe UTF-8/XML-entity boundaries).
//! - Server replies with TEXT messages (`Path: audio.metadata` /
//!   `turn.start` / `turn.end` / `response`) and BINARY messages whose
//!   first 2 bytes (big-endian) are the header length, then headers +
//!   MP3 data (`Path: audio`, `Content-Type: audio/mpeg`). A final binary
//!   message with no Content-Type and no data ends the stream.
//! - No audio received is an error. A 403 handshake is retried once after
//!   adjusting the clock skew from the server's `Date` header.

use crate::api::{BoxFuture, TtsEngine, TtsError, TtsRetryAttempt, TtsSynthesis, TTS_MAX_RETRIES};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

/// The trusted client token doubles as the subscription key (public
/// constant from the `edge-tts` source; it is the Edge browser's key).
const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
const WSS_BASE: &str = "wss://api.msedgeservices.com/tts/cognitiveservices/websocket/v1";
const SEC_MS_GEC_VERSION: &str = "1-140.0.3485.14";
const ORIGIN: &str = "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
/// Maximum SSML text chunk size in bytes (from `edge-tts`).
const MAX_CHUNK_BYTES: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(60);
/// First retry backoff (ms); doubled per retry, capped below.
const RETRY_BASE_DELAY_MS: u64 = 500;
/// Maximum backoff between retries (ms).
const RETRY_MAX_DELAY_MS: u64 = 4000;
/// Windows file-time epoch offset (seconds between 1601-01-01 and 1970-01-01).
const WIN_EPOCH_SECS: f64 = 11_644_473_600.0;

/// `Sec-MS-GEC` token for a given Unix timestamp (seconds, float).
/// Pure function — deterministic at a fixed timestamp (unit-tested).
pub fn sec_ms_gec_for(unix_timestamp: f64) -> String {
    let mut ticks = unix_timestamp + WIN_EPOCH_SECS;
    ticks -= ticks % 300.0;
    ticks *= 1e9 / 100.0;
    let to_hash = format!("{ticks:.0}{TRUSTED_CLIENT_TOKEN}");
    let digest = Sha256::digest(to_hash.as_bytes());
    format!("{digest:X}")
}

/// JavaScript-style date string in UTC, as Edge sends it:
/// `Tue Nov 22 2026 15:04:05 GMT+0000 (Coordinated Universal Time)`.
pub fn js_date_string(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)")
        .to_string()
}

/// Remove characters the service rejects (control chars 0–8, 11–12, 14–31).
pub fn remove_incompatible_characters(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if (('\u{0}'..='\u{8}').contains(&ch))
                || ch == '\u{b}'
                || ch == '\u{c}'
                || (('\u{e}'..='\u{1f}').contains(&ch))
            {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

/// XML-escape text for the SSML body.
pub fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Split escaped text into chunks of at most `MAX_CHUNK_BYTES` bytes,
/// never splitting a UTF-8 multi-byte character or an XML entity
/// (`&amp;` etc.), and stripping each chunk (mirrors
/// `split_text_by_byte_length` in `edge-tts`).
pub fn split_text_by_byte_length(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        let mut end = (start + MAX_CHUNK_BYTES).min(bytes.len());
        // Back off to a UTF-8 character boundary.
        while end > start && end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        // Back off if the chunk ends inside an XML entity (`&...` with no
        // `;` before the split).
        if end > start {
            if let Some(amp) = text[start..end].rfind('&') {
                let after = &text[start + amp..end];
                if !after.contains(';') {
                    end = start + amp;
                }
            }
        }
        if end == start {
            // A single character (or entity) exceeds the limit; advance by
            // one character to avoid an infinite loop.
            end = start + text[start..].chars().next().map_or(1, |c| c.len_utf8());
        }
        let chunk = text[start..end].trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        start = end;
    }
    chunks
}

/// Expand a short voice id (`vi-VN-NamMinhNeural`) to the full name the
/// service expects (`Microsoft Server Speech Text to Speech Voice (vi-VN,
/// NamMinhNeural)`), mirroring `TTSConfig.__post_init__` in `edge-tts`.
/// A voice already in the full form (or not matching the short pattern) is
/// returned unchanged. The short form is rejected by the service: it closes
/// the connection with "Unsupported Edge output format:
/// audio-16khz-32kbitrate-mono-mp3."
pub fn full_voice_name(voice: &str) -> String {
    let mut parts = voice.splitn(3, '-');
    let Some(lang) = parts.next() else {
        return voice.to_string();
    };
    let Some(region) = parts.next() else {
        return voice.to_string();
    };
    let Some(mut name) = parts.next() else {
        return voice.to_string();
    };
    if lang.len() < 2
        || !lang.bytes().all(|b| b.is_ascii_lowercase())
        || region.len() < 2
        || !region.bytes().all(|b| b.is_ascii_uppercase())
        || !name.ends_with("Neural")
    {
        return voice.to_string();
    }
    let mut region = region.to_string();
    if let Some(dash) = name.find('-') {
        region = format!("{region}-{}", &name[..dash]);
        name = &name[dash + 1..];
    }
    format!("Microsoft Server Speech Text to Speech Voice ({lang}-{region}, {name})")
}

/// Build the SSML document for one chunk.
pub fn build_ssml(voice: &str, rate: &str, volume: &str, escaped_text: &str) -> String {
    let voice = full_voice_name(voice);
    format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'><voice name='{voice}'><prosody pitch='+0Hz' rate='{rate}' volume='{volume}'>{escaped_text}</prosody></voice></speak>"
    )
}

/// The `speech.config` client message. Brace placement matters:
/// `outputFormat` must sit INSIDE `audio` (a sibling of `metadataoptions`).
/// An earlier version closed `audio` before `outputFormat`, which made the
/// server fall back to its default `audio-16khz-32kbitrate-mono-mp3` and
/// close the connection with "Unsupported Edge output format".
fn speech_config_message() -> String {
    format!(
        "X-Timestamp:{}\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n{{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":{{\"sentenceBoundaryEnabled\":\"true\",\"wordBoundaryEnabled\":\"false\"}},\"outputFormat\":\"audio-24khz-48kbitrate-mono-mp3\"}}}}}}}}\r\n",
        js_date_string(chrono::Utc::now())
    )
}

/// The SSML client message. The trailing `Z` after the timestamp is an
/// intentional Edge bug, kept verbatim.
fn ssml_message(request_id: &str, ssml: &str) -> String {
    format!(
        "X-RequestId:{request_id}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{}Z\r\nPath:ssml\r\n\r\n{ssml}",
        js_date_string(chrono::Utc::now())
    )
}

/// Parse `Key:Value` header lines (separated by `\r\n`) into pairs.
/// The first line of a BINARY message carries the 2 length bytes as a
/// prefix (the `edge-tts` Python client has the same quirk); that key is
/// never looked up, so it is harmless.
fn parse_headers(header_bytes: &[u8]) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    for line in header_bytes.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(colon) = line.iter().position(|b| *b == b':') {
            let key = String::from_utf8_lossy(&line[..colon]).trim().to_string();
            let value = String::from_utf8_lossy(&line[colon + 1..])
                .trim()
                .to_string();
            headers.push((key, value));
        }
    }
    headers
}

/// Split a BINARY server message into headers + audio data. Layout
/// (verified live against the real service): `[2 bytes: H][H-2 bytes of
/// header text][\r\n][audio]`, where H is the big-endian header length
/// (it counts the 2 length bytes, so the header text is `data[2..H]` and
/// the audio starts at `H + 2`).
fn split_headers_and_data(data: &[u8], header_length: usize) -> (Vec<(String, String)>, &[u8]) {
    let headers = parse_headers(&data[..header_length.min(data.len())]);
    let body_start = header_length + 2;
    let body = if body_start <= data.len() {
        &data[body_start..]
    } else {
        &[]
    };
    (headers, body)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Parse an RFC 1123 `Date` header (`Tue, 22 Nov 2026 15:04:05 GMT`) to a
/// Unix timestamp, for the 403 clock-skew retry.
fn parse_server_date(date: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc2822(date)
        .ok()
        .map(|dt| dt.timestamp() as f64)
}

/// Native Edge-TTS engine. Stateless except for the clock-skew correction
/// learned from a 403 response (shared across calls).
pub struct EdgeTtsEngine {
    clock_skew_secs: Mutex<f64>,
}

impl Default for EdgeTtsEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl EdgeTtsEngine {
    pub fn new() -> Self {
        Self {
            clock_skew_secs: Mutex::new(0.0),
        }
    }

    fn connect_url(&self) -> String {
        let skew = *self
            .clock_skew_secs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
            + skew;
        let connection_id = uuid::Uuid::new_v4().simple().to_string();
        format!(
            "{WSS_BASE}?Ocp-Apim-Subscription-Key={TRUSTED_CLIENT_TOKEN}&ConnectionId={connection_id}&Sec-MS-GEC={}&Sec-MS-GEC-Version={SEC_MS_GEC_VERSION}",
            sec_ms_gec_for(now)
        )
    }

    fn build_request(&self) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, TtsError> {
        self.connect_url()
            .into_client_request()
            .map_err(|err| TtsError::Network {
                detail: format!("không tạo được yêu cầu TTS: {err}"),
                retries: Vec::new(),
            })
            .and_then(|mut req| {
                let headers = req.headers_mut();
                headers.insert(
                    "Pragma",
                    "no-cache".parse().map_err(|_| TtsError::InvalidInput {
                        detail: "header Pragma không hợp lệ".to_string(),
                    })?,
                );
                headers.insert(
                    "Cache-Control",
                    "no-cache".parse().map_err(|_| TtsError::InvalidInput {
                        detail: "header Cache-Control không hợp lệ".to_string(),
                    })?,
                );
                headers.insert(
                    "Origin",
                    ORIGIN.parse().map_err(|_| TtsError::InvalidInput {
                        detail: "header Origin không hợp lệ".to_string(),
                    })?,
                );
                headers.insert(
                    "Sec-WebSocket-Version",
                    "13".parse().map_err(|_| TtsError::InvalidInput {
                        detail: "header Sec-WebSocket-Version không hợp lệ".to_string(),
                    })?,
                );
                headers.insert(
                    "User-Agent",
                    USER_AGENT.parse().map_err(|_| TtsError::InvalidInput {
                        detail: "header User-Agent không hợp lệ".to_string(),
                    })?,
                );
                headers.insert(
                    "Accept-Encoding",
                    "gzip, deflate, br"
                        .parse()
                        .map_err(|_| TtsError::InvalidInput {
                            detail: "header Accept-Encoding không hợp lệ".to_string(),
                        })?,
                );
                headers.insert(
                    "Accept-Language",
                    "en-US,en;q=0.9"
                        .parse()
                        .map_err(|_| TtsError::InvalidInput {
                            detail: "header Accept-Language không hợp lệ".to_string(),
                        })?,
                );
                Ok(req)
            })
    }

    /// One synthesis attempt (a fresh WebSocket per call, like `edge-tts`).
    /// Returns `Ok(None)` when the handshake was a 403 so the caller can
    /// adjust the clock skew and retry once.
    async fn synthesize_once(
        &self,
        text: &str,
        voice: &str,
        rate: &str,
    ) -> Result<Option<Vec<u8>>, TtsError> {
        let request = self.build_request()?;
        let (mut ws, _) = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(request),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => {
                if let tokio_tungstenite::tungstenite::Error::Http(resp) = &err {
                    if resp.status() == 403 {
                        if let Some(date) = resp.headers().get("Date").and_then(|v| v.to_str().ok())
                        {
                            if let Some(server_ts) = parse_server_date(date) {
                                let client_ts = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs_f64())
                                    .unwrap_or(0.0);
                                let skew = server_ts - client_ts;
                                *self
                                    .clock_skew_secs
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()) = skew;
                                return Ok(None);
                            }
                        }
                        return Err(TtsError::Upstream {
                            status: 403,
                            detail: "Edge-TTS từ chối kết nối (403) và không có header Date để chỉnh lệch đồng hồ".to_string(),
                            retries: Vec::new(),
                        });
                    }
                    return Err(TtsError::Upstream {
                        status: resp.status().as_u16(),
                        detail: format!("Edge-TTS từ chối kết nối: {err}"),
                        retries: Vec::new(),
                    });
                }
                return Err(TtsError::Network {
                    detail: format!("không kết nối được Edge-TTS: {err}"),
                    retries: Vec::new(),
                });
            }
            Err(_) => {
                return Err(TtsError::Timeout {
                    timeout_ms: CONNECT_TIMEOUT.as_millis() as u64,
                });
            }
        };

        ws.send(Message::Text(speech_config_message().into()))
            .await
            .map_err(|err| TtsError::Network {
                detail: format!("không gửi được speech.config: {err}"),
                retries: Vec::new(),
            })?;

        let cleaned = remove_incompatible_characters(text);
        let escaped = escape_xml(&cleaned);
        let chunks = split_text_by_byte_length(&escaped);
        if chunks.is_empty() {
            return Err(TtsError::EmptyInput);
        }

        let mut audio: Vec<u8> = Vec::new();
        for chunk in &chunks {
            let request_id = uuid::Uuid::new_v4().simple().to_string();
            let ssml = build_ssml(voice, rate, "+0%", chunk);
            ws.send(Message::Text(ssml_message(&request_id, &ssml).into()))
                .await
                .map_err(|err| TtsError::Network {
                    detail: format!("không gửi được yêu cầu SSML: {err}"),
                    retries: Vec::new(),
                })?;
            // Read until `turn.end` for this chunk.
            loop {
                let msg = match tokio::time::timeout(RECEIVE_TIMEOUT, ws.next()).await {
                    Ok(Some(Ok(msg))) => msg,
                    Ok(Some(Err(err))) => {
                        return Err(TtsError::Network {
                            detail: format!("lỗi đọc phản hồi Edge-TTS: {err}"),
                            retries: Vec::new(),
                        });
                    }
                    Ok(None) => {
                        return Err(TtsError::InvalidResponse {
                            detail: "Edge-TTS đóng kết nối giữa chừng".to_string(),
                        });
                    }
                    Err(_) => {
                        return Err(TtsError::Timeout {
                            timeout_ms: RECEIVE_TIMEOUT.as_millis() as u64,
                        });
                    }
                };
                match msg {
                    Message::Text(text_msg) => {
                        let bytes = text_msg.as_bytes();
                        let Some(sep) = find_double_crlf(bytes) else {
                            return Err(TtsError::InvalidResponse {
                                detail: "tin nhắn TEXT của Edge-TTS thiếu phân cách header"
                                    .to_string(),
                            });
                        };
                        let headers = parse_headers(&bytes[..sep]);
                        match header(&headers, "Path") {
                            Some("turn.end") => break,
                            Some("audio.metadata") | Some("turn.start") | Some("response") => {}
                            Some(other) => {
                                return Err(TtsError::InvalidResponse {
                                    detail: format!("Edge-TTS trả về Path không rõ: {other}"),
                                });
                            }
                            None => {
                                return Err(TtsError::InvalidResponse {
                                    detail: "tin nhắn TEXT của Edge-TTS thiếu Path".to_string(),
                                });
                            }
                        }
                    }
                    Message::Binary(bin) => {
                        if bin.len() < 2 {
                            return Err(TtsError::InvalidResponse {
                                detail: "tin nhắn BINARY của Edge-TTS thiếu độ dài header"
                                    .to_string(),
                            });
                        }
                        let header_len = u16::from_be_bytes([bin[0], bin[1]]) as usize;
                        if header_len > bin.len() {
                            return Err(TtsError::InvalidResponse {
                                detail: "độ dài header BINARY vượt quá độ dài tin nhắn".to_string(),
                            });
                        }
                        let (headers, data) = split_headers_and_data(&bin, header_len);
                        if header(&headers, "Path") != Some("audio") {
                            return Err(TtsError::InvalidResponse {
                                detail: "tin nhắn BINARY không phải audio".to_string(),
                            });
                        }
                        match header(&headers, "Content-Type") {
                            Some("audio/mpeg") => {
                                if data.is_empty() {
                                    return Err(TtsError::InvalidResponse {
                                        detail: "tin nhắn audio rỗng".to_string(),
                                    });
                                }
                                audio.extend_from_slice(data);
                            }
                            None => {
                                // Stream-end marker: no Content-Type and no
                                // data is expected; data here would be an error.
                                if !data.is_empty() {
                                    return Err(TtsError::InvalidResponse {
                                        detail:
                                            "tin nhắn BINARY không có Content-Type nhưng có dữ liệu"
                                                .to_string(),
                                    });
                                }
                            }
                            Some(other) => {
                                return Err(TtsError::InvalidResponse {
                                    detail: format!("Content-Type audio không rõ: {other}"),
                                });
                            }
                        }
                    }
                    Message::Close(_) => {
                        return Err(TtsError::InvalidResponse {
                            detail: "Edge-TTS đóng kết nối giữa chừng".to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }

        if audio.is_empty() {
            return Err(TtsError::InvalidResponse {
                detail: "Edge-TTS không trả về âm thanh nào".to_string(),
            });
        }
        Ok(Some(audio))
    }
}

fn find_double_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Rate validation: `[+-]\d{1,3}%`, |rate| ≤ 100 (from `edge-tts`).
fn valid_rate(rate: &str) -> bool {
    let Some(rest) = rate.strip_prefix(['+', '-']) else {
        return false;
    };
    let Some(digits) = rest.strip_suffix('%') else {
        return false;
    };
    !digits.is_empty()
        && digits.len() <= 3
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits.parse::<u32>().is_ok_and(|n| n <= 100)
}

/// Backoff before retry number `retry` (1-based): 500 ms, 1 s, 2 s,
/// capped at 4 s.
fn backoff_delay_ms(retry: u32) -> u64 {
    RETRY_BASE_DELAY_MS
        .saturating_mul(1u64 << retry.saturating_sub(1))
        .min(RETRY_MAX_DELAY_MS)
}

/// One delay per permitted retry (`TTS_MAX_RETRIES` entries).
fn retry_delay_schedule() -> Vec<u64> {
    (1..=TTS_MAX_RETRIES).map(backoff_delay_ms).collect()
}

/// Only transient failures are retried: HTTP 429 (rate limit) and 5xx,
/// plus network errors with no response. Other 4xx are not — 403 has its
/// own clock-skew retry inside the attempt, and the rest are client bugs.
/// Timeouts are not retried either: a 60 s receive timeout retried three
/// times would stall the realtime pipeline for minutes.
fn is_retryable(err: &TtsError) -> bool {
    match err {
        TtsError::Upstream { status, .. } => *status == 429 || (500..600).contains(status),
        TtsError::Network { .. } => true,
        _ => false,
    }
}

fn retry_attempt_from(err: &TtsError, attempt: u32, backoff_ms: u64) -> TtsRetryAttempt {
    TtsRetryAttempt {
        attempt,
        status: match err {
            TtsError::Upstream { status, .. } => Some(*status),
            _ => None,
        },
        detail: err.to_string(),
        backoff_ms,
    }
}

/// Run `attempt` until it succeeds, retrying transient failures with the
/// given backoff schedule (`delays_ms[i]` is waited before retry i+1).
/// Returns the attempt outcome plus every retry that was taken, so the
/// caller can report them (the pipeline logs them to the subtitle log
/// file; a retried-then-recovered 429 stays visible instead of silent).
async fn retry_with_backoff<F, Fut>(
    mut attempt: F,
    delays_ms: &[u64],
) -> (Result<Vec<u8>, TtsError>, Vec<TtsRetryAttempt>)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Vec<u8>, TtsError>> + Send,
{
    let mut retries = Vec::new();
    let mut attempt_no = 0u32;
    loop {
        match attempt().await {
            Ok(audio) => return (Ok(audio), retries),
            Err(err) => {
                let idx = attempt_no as usize;
                if !is_retryable(&err) || idx >= delays_ms.len() {
                    return (Err(err), retries);
                }
                let backoff_ms = delays_ms[idx];
                retries.push(retry_attempt_from(&err, attempt_no + 1, backoff_ms));
                attempt_no += 1;
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
        }
    }
}

impl TtsEngine for EdgeTtsEngine {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<TtsSynthesis, TtsError>> {
        let text = text.to_string();
        let voice = voice.to_string();
        let rate = rate.to_string();
        Box::pin(async move {
            if text.trim().is_empty() {
                return Err(TtsError::EmptyInput);
            }
            if voice.trim().is_empty() {
                return Err(TtsError::InvalidInput {
                    detail: "giọng đọc TTS trống".to_string(),
                });
            }
            if !valid_rate(&rate) {
                return Err(TtsError::InvalidInput {
                    detail: format!("tốc độ TTS không hợp lệ: {rate}"),
                });
            }
            let delays = retry_delay_schedule();
            let (result, retries) =
                retry_with_backoff(|| self.synthesize_attempt(&text, &voice, &rate), &delays).await;
            match result {
                Ok(audio) => Ok(TtsSynthesis { audio, retries }),
                Err(mut err) => {
                    err.set_retries(retries);
                    Err(err)
                }
            }
        })
    }
}

impl EdgeTtsEngine {
    /// One synthesis attempt, including the single immediate re-try after
    /// a 403 clock-skew adjustment. The caller (`synthesize`) owns the
    /// backoff retry loop for transient failures (429 / 5xx / network).
    async fn synthesize_attempt(
        &self,
        text: &str,
        voice: &str,
        rate: &str,
    ) -> Result<Vec<u8>, TtsError> {
        match self.synthesize_once(text, voice, rate).await? {
            Some(audio) => Ok(audio),
            None => {
                // 403 once: clock skew adjusted, retry a single time.
                match self.synthesize_once(text, voice, rate).await? {
                    Some(audio) => Ok(audio),
                    None => Err(TtsError::Upstream {
                        status: 403,
                        detail: "Edge-TTS từ chối kết nối (403) cả sau khi chỉnh lệch đồng hồ"
                            .to_string(),
                        retries: Vec::new(),
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sec_ms_gec_is_deterministic_at_a_fixed_timestamp() {
        // Same 300 s bucket (1_700_000_000 % 300 == 200, so +99 stays
        // in the bucket) → same token.
        let a = sec_ms_gec_for(1_700_000_000.0);
        let b = sec_ms_gec_for(1_700_000_099.0);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
        // The next 300 s bucket → a different token.
        assert_ne!(a, sec_ms_gec_for(1_700_000_100.0));
    }

    #[test]
    fn js_date_string_matches_edge_format() {
        let dt = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(
            js_date_string(dt),
            "Tue Nov 14 2023 22:13:20 GMT+0000 (Coordinated Universal Time)"
        );
    }

    #[test]
    fn escape_xml_escapes_special_chars() {
        assert_eq!(
            escape_xml("a & b <c> \"d\" 'e'"),
            "a &amp; b &lt;c&gt; &quot;d&quot; &apos;e&apos;"
        );
    }

    #[test]
    fn remove_incompatible_characters_replaces_control_chars() {
        assert_eq!(
            remove_incompatible_characters("a\u{1}b\u{b}c\u{1f}d\te"),
            "a b c d\te"
        );
    }

    #[test]
    fn split_text_keeps_short_text_whole() {
        let chunks = split_text_by_byte_length("Xin chào, tôi là Tiểu Minh.");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "Xin chào, tôi là Tiểu Minh.");
    }

    #[test]
    fn split_text_never_splits_utf8_or_entities() {
        let long = "你好".repeat(3000); // 9000 bytes, all multi-byte
        let chunks = split_text_by_byte_length(&long);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= MAX_CHUNK_BYTES);
            assert_eq!(chunk.chars().count(), chunk.len() / 3);
        }
        let with_entity = format!("{}&amp;{}", "a".repeat(4090), "b".repeat(100));
        let chunks = split_text_by_byte_length(&with_entity);
        for chunk in &chunks {
            assert!(!chunk.ends_with('&'));
            assert!(chunk.len() <= MAX_CHUNK_BYTES);
        }
    }

    #[test]
    fn full_voice_name_expands_short_ids() {
        assert_eq!(
            full_voice_name("vi-VN-NamMinhNeural"),
            "Microsoft Server Speech Text to Speech Voice (vi-VN, NamMinhNeural)"
        );
        assert_eq!(
            full_voice_name("zh-CN-XiaoxiaoNeural"),
            "Microsoft Server Speech Text to Speech Voice (zh-CN, XiaoxiaoNeural)"
        );
        assert_eq!(
            full_voice_name("vi-VN-HoaiMyNeural"),
            "Microsoft Server Speech Text to Speech Voice (vi-VN, HoaiMyNeural)"
        );
        // Already full, or not matching the short pattern: unchanged.
        let full = "Microsoft Server Speech Text to Speech Voice (vi-VN, NamMinhNeural)";
        assert_eq!(full_voice_name(full), full);
        assert_eq!(full_voice_name("not-a-voice"), "not-a-voice");
    }

    #[test]
    fn speech_config_puts_output_format_inside_audio() {
        let msg = speech_config_message();
        let body = msg.split("\r\n\r\n").nth(1).expect("body after headers");
        // Byte-identical to the `edge-tts` Python client's JSON body.
        assert_eq!(
            body.trim_end(),
            "{\"context\":{\"synthesis\":{\"audio\":{\"metadataoptions\":{\"sentenceBoundaryEnabled\":\"true\",\"wordBoundaryEnabled\":\"false\"},\"outputFormat\":\"audio-24khz-48kbitrate-mono-mp3\"}}}}"
        );
    }

    #[test]
    fn build_ssml_has_voice_rate_volume_and_text() {
        let ssml = build_ssml("vi-VN-NamMinhNeural", "+0%", "+0%", "Xin chào");
        assert!(ssml.contains(
            "name='Microsoft Server Speech Text to Speech Voice (vi-VN, NamMinhNeural)'"
        ));
        assert!(ssml.contains("rate='+0%'"));
        assert!(ssml.contains("volume='+0%'"));
        assert!(ssml.contains(">Xin chào<"));
    }

    #[test]
    fn split_headers_and_data_parses_binary_audio() {
        // Real layout (captured live): [2 bytes: H=128][126 bytes of
        // header text][\r\n][MP3 audio starting with the frame sync].
        let header_text = b"X-RequestId:a52965af14c34972ab38887c5a619518\r\nContent-Type:audio/mpeg\r\nX-StreamId:13AC94EDA1454BBC99D69C2FA89DCFEA\r\nPath:audio";
        assert_eq!(header_text.len(), 126);
        let mut msg = vec![0u8, 128u8];
        msg.extend_from_slice(header_text);
        msg.extend_from_slice(b"\r\n");
        msg.extend_from_slice(&[0xff, 0xf3, 0x64, 0xc4]);
        let (headers, body) = split_headers_and_data(&msg, 128);
        assert_eq!(header(&headers, "Path"), Some("audio"));
        assert_eq!(header(&headers, "Content-Type"), Some("audio/mpeg"));
        assert_eq!(body, &[0xff, 0xf3, 0x64, 0xc4]);
    }

    #[test]
    fn parse_headers_reads_text_message_path() {
        let msg = b"Path:turn.end\r\nX-RequestId:abc";
        let headers = parse_headers(msg);
        assert_eq!(header(&headers, "Path"), Some("turn.end"));
    }

    #[test]
    fn valid_rate_accepts_edge_tts_range() {
        assert!(valid_rate("+0%"));
        assert!(valid_rate("-10%"));
        assert!(valid_rate("+100%"));
        assert!(!valid_rate("0%"));
        assert!(!valid_rate("+101%"));
        assert!(!valid_rate("+0"));
        assert!(!valid_rate("fast"));
    }

    #[test]
    fn backoff_delay_ms_grows_exponentially_and_caps() {
        assert_eq!(backoff_delay_ms(1), 500);
        assert_eq!(backoff_delay_ms(2), 1000);
        assert_eq!(backoff_delay_ms(3), 2000);
        assert_eq!(backoff_delay_ms(4), 4000);
        assert_eq!(backoff_delay_ms(10), 4000);
        assert_eq!(
            retry_delay_schedule(),
            vec![500, 1000, 2000],
            "one delay per permitted retry (TTS_MAX_RETRIES = 3)"
        );
    }

    #[test]
    fn is_retryable_only_for_transient_failures() {
        let upstream = |status: u16| TtsError::Upstream {
            status,
            detail: "x".to_string(),
            retries: Vec::new(),
        };
        assert!(is_retryable(&upstream(429)));
        assert!(is_retryable(&upstream(500)));
        assert!(is_retryable(&upstream(502)));
        assert!(is_retryable(&upstream(503)));
        assert!(!is_retryable(&upstream(400)));
        assert!(!is_retryable(&upstream(403)));
        assert!(!is_retryable(&upstream(404)));
        assert!(is_retryable(&TtsError::Network {
            detail: "reset".to_string(),
            retries: Vec::new(),
        }));
        assert!(!is_retryable(&TtsError::Timeout { timeout_ms: 10_000 }));
        assert!(!is_retryable(&TtsError::InvalidResponse {
            detail: "x".to_string(),
        }));
        assert!(!is_retryable(&TtsError::EmptyInput));
    }

    /// Scripted fake attempt for `retry_with_backoff`: pops a scripted
    /// outcome per call and counts calls. No network involved.
    struct ScriptedAttempt {
        scripted: std::sync::Mutex<std::collections::VecDeque<Result<Vec<u8>, TtsError>>>,
        calls: std::sync::atomic::AtomicU32,
    }

    impl ScriptedAttempt {
        fn new(scripted: Vec<Result<Vec<u8>, TtsError>>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                scripted: std::sync::Mutex::new(scripted.into()),
                calls: std::sync::atomic::AtomicU32::new(0),
            })
        }

        fn calls(&self) -> u32 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn err_429() -> TtsError {
        TtsError::Upstream {
            status: 429,
            detail: "Too Many Requests".to_string(),
            retries: Vec::new(),
        }
    }

    #[tokio::test]
    async fn retry_with_backoff_recovers_after_429s() {
        let attempt = ScriptedAttempt::new(vec![
            Err(err_429()),
            Err(err_429()),
            Ok(vec![0xff, 0xfb, 0x01]),
        ]);
        let attempt_for_call = std::sync::Arc::clone(&attempt);
        let (result, retries) = retry_with_backoff(
            move || {
                let attempt = std::sync::Arc::clone(&attempt_for_call);
                async move {
                    attempt
                        .calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    attempt
                        .scripted
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pop_front()
                        .unwrap_or_else(|| Ok(Vec::new()))
                }
            },
            &[0, 0, 0],
        )
        .await;
        assert_eq!(result.expect("synthesis recovers"), vec![0xff, 0xfb, 0x01]);
        assert_eq!(attempt.calls(), 3);
        assert_eq!(retries.len(), 2);
        assert_eq!(retries[0].attempt, 1);
        assert_eq!(retries[0].status, Some(429));
        assert_eq!(retries[0].backoff_ms, 0);
        assert_eq!(retries[1].attempt, 2);
        assert_eq!(retries[1].status, Some(429));
    }

    #[tokio::test]
    async fn retry_with_backoff_gives_up_after_max_retries() {
        let attempt = ScriptedAttempt::new(vec![
            Err(err_429()),
            Err(err_429()),
            Err(err_429()),
            Err(err_429()),
            Err(err_429()),
        ]);
        let attempt_for_call = std::sync::Arc::clone(&attempt);
        let (result, retries) = retry_with_backoff(
            move || {
                let attempt = std::sync::Arc::clone(&attempt_for_call);
                async move {
                    attempt
                        .calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    attempt
                        .scripted
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pop_front()
                        .unwrap_or_else(|| Ok(Vec::new()))
                }
            },
            &[0, 0, 0],
        )
        .await;
        assert!(matches!(
            result,
            Err(TtsError::Upstream { status: 429, .. })
        ));
        assert_eq!(attempt.calls(), 4, "initial attempt + 3 retries");
        assert_eq!(retries.len(), 3);
        assert_eq!(retries[2].attempt, 3);
    }

    #[tokio::test]
    async fn retry_with_backoff_does_not_retry_client_errors() {
        let attempt = ScriptedAttempt::new(vec![Err(TtsError::Upstream {
            status: 400,
            detail: "bad request".to_string(),
            retries: Vec::new(),
        })]);
        let attempt_for_call = std::sync::Arc::clone(&attempt);
        let (result, retries) = retry_with_backoff(
            move || {
                let attempt = std::sync::Arc::clone(&attempt_for_call);
                async move {
                    attempt
                        .calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    attempt
                        .scripted
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pop_front()
                        .unwrap_or_else(|| Ok(Vec::new()))
                }
            },
            &[0, 0, 0],
        )
        .await;
        assert!(matches!(
            result,
            Err(TtsError::Upstream { status: 400, .. })
        ));
        assert_eq!(attempt.calls(), 1);
        assert!(retries.is_empty());
    }
}
