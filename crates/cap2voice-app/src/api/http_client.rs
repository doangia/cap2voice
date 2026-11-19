//! `reqwest` 0.13 HTTP clients implementing `Translator`, `TtsEngine`, and `/health`.

use super::{
    ApiErrorBody, BoxFuture, HealthError, HealthStatus, TranslateError, TranslateRequest,
    TranslateResponse, Translator, TtsEngine, TtsError, TtsRequest,
};
use std::time::Duration;

/// TCP connect timeout for localhost API server requests (`docs/spec.md` §3.3).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Request timeout for `POST /translate` (`docs/spec.md` §3.3).
pub const DEFAULT_TRANSLATE_TIMEOUT: Duration = Duration::from_secs(8);

/// Request timeout for `POST /tts` (`docs/spec.md` §3.4).
pub const DEFAULT_TTS_TIMEOUT: Duration = Duration::from_secs(10);

/// Request timeout for manual `GET /health` checks.
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) fn build_local_http_client(
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        .map_err(|e| e.to_string())
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn extract_error_detail(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorBody>(body) {
        let trimmed = parsed.detail.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty error body".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Probe `GET {base_url}/health` using a caller-provided client.
pub(crate) async fn check_health_with_client(
    client: &reqwest::Client,
    base_url: &str,
    timeout: Duration,
) -> Result<HealthStatus, HealthError> {
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|err| {
            if err.is_timeout() {
                HealthError::Timeout {
                    timeout_ms: duration_ms(timeout),
                }
            } else {
                HealthError::ServerUnavailable {
                    detail: err.to_string(),
                }
            }
        })?;

    let status = response.status();
    let body = response.text().await.map_err(|err| {
        if err.is_timeout() {
            HealthError::Timeout {
                timeout_ms: duration_ms(timeout),
            }
        } else {
            HealthError::InvalidResponse {
                detail: err.to_string(),
            }
        }
    })?;

    if !status.is_success() {
        return Err(HealthError::UnexpectedStatus {
            status: status.as_u16(),
            detail: extract_error_detail(&body),
        });
    }

    let parsed: HealthStatus =
        serde_json::from_str(&body).map_err(|err| HealthError::InvalidResponse {
            detail: format!("JSON không hợp lệ: {err}"),
        })?;
    if parsed.status != "ok" {
        return Err(HealthError::InvalidResponse {
            detail: format!("trạng thái server là '{}' thay vì 'ok'", parsed.status),
        });
    }
    Ok(parsed)
}

/// Probe `GET {base_url}/health` to verify local Python server readiness.
pub async fn check_health(base_url: &str, timeout: Duration) -> Result<HealthStatus, HealthError> {
    let connect_timeout = DEFAULT_CONNECT_TIMEOUT.min(timeout);
    let client = build_local_http_client(connect_timeout, timeout)
        .map_err(|detail| HealthError::ServerUnavailable { detail })?;
    check_health_with_client(&client, base_url, timeout).await
}

/// HTTP client calling `POST /translate` on the managed local server.
#[derive(Debug, Clone)]
pub struct HttpTranslator {
    base_url: String,
    client: reqwest::Client,
    request_timeout: Duration,
}

impl HttpTranslator {
    pub fn for_port(port: u16) -> Result<Self, TranslateError> {
        Self::with_base_url(
            format!("http://127.0.0.1:{port}"),
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_TRANSLATE_TIMEOUT,
        )
    }

    pub fn with_base_url(
        base_url: impl Into<String>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, TranslateError> {
        let client = build_local_http_client(connect_timeout, request_timeout)
            .map_err(|detail| TranslateError::ServerUnavailable { detail })?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(Self {
            base_url,
            client,
            request_timeout,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Translator for HttpTranslator {
    fn translate<'a>(&'a self, chinese: &'a str) -> BoxFuture<'a, Result<String, TranslateError>> {
        Box::pin(async move {
            let trimmed = chinese.trim();
            if trimmed.is_empty() {
                return Err(TranslateError::EmptyInput);
            }

            let url = format!("{}/translate", self.base_url);
            let payload = TranslateRequest {
                text: trimmed.to_string(),
            };

            let response = self
                .client
                .post(&url)
                .json(&payload)
                .send()
                .await
                .map_err(|err| {
                    if err.is_timeout() {
                        TranslateError::Timeout {
                            timeout_ms: duration_ms(self.request_timeout),
                        }
                    } else {
                        TranslateError::ServerUnavailable {
                            detail: err.to_string(),
                        }
                    }
                })?;

            let status = response.status();
            let body = response.text().await.map_err(|err| {
                if err.is_timeout() {
                    TranslateError::Timeout {
                        timeout_ms: duration_ms(self.request_timeout),
                    }
                } else {
                    TranslateError::InvalidResponse {
                        detail: err.to_string(),
                    }
                }
            })?;

            match status.as_u16() {
                200 => {
                    let parsed: TranslateResponse = serde_json::from_str(&body).map_err(|err| {
                        TranslateError::InvalidResponse {
                            detail: format!("JSON không hợp lệ: {err}"),
                        }
                    })?;
                    let out = parsed.text.trim();
                    if out.is_empty() {
                        return Err(TranslateError::InvalidResponse {
                            detail: "kết quả dịch trả về rỗng".to_string(),
                        });
                    }
                    Ok(out.to_string())
                }
                400 => Err(TranslateError::InvalidInput {
                    detail: extract_error_detail(&body),
                }),
                429 => Err(TranslateError::RateLimited {
                    detail: extract_error_detail(&body),
                }),
                code => Err(TranslateError::Upstream {
                    status: code,
                    detail: extract_error_detail(&body),
                }),
            }
        })
    }
}

/// HTTP client calling `POST /tts` on the managed local server.
#[derive(Debug, Clone)]
pub struct HttpTtsEngine {
    base_url: String,
    client: reqwest::Client,
    request_timeout: Duration,
}

impl HttpTtsEngine {
    pub fn for_port(port: u16) -> Result<Self, TtsError> {
        Self::with_base_url(
            format!("http://127.0.0.1:{port}"),
            DEFAULT_CONNECT_TIMEOUT,
            DEFAULT_TTS_TIMEOUT,
        )
    }

    pub fn with_base_url(
        base_url: impl Into<String>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, TtsError> {
        let client = build_local_http_client(connect_timeout, request_timeout)
            .map_err(|detail| TtsError::ServerUnavailable { detail })?;
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Ok(Self {
            base_url,
            client,
            request_timeout,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl TtsEngine for HttpTtsEngine {
    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        voice: &'a str,
        rate: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, TtsError>> {
        Box::pin(async move {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return Err(TtsError::EmptyInput);
            }

            let url = format!("{}/tts", self.base_url);
            let payload = TtsRequest {
                text: trimmed.to_string(),
                voice: voice.to_string(),
                rate: rate.to_string(),
            };

            let response = self
                .client
                .post(&url)
                .json(&payload)
                .send()
                .await
                .map_err(|err| {
                    if err.is_timeout() {
                        TtsError::Timeout {
                            timeout_ms: duration_ms(self.request_timeout),
                        }
                    } else {
                        TtsError::ServerUnavailable {
                            detail: err.to_string(),
                        }
                    }
                })?;

            let status = response.status();
            let content_type_is_audio = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.to_ascii_lowercase().starts_with("audio/"));

            if status.as_u16() != 200 {
                let body = response.text().await.unwrap_or_default();
                return match status.as_u16() {
                    400 => Err(TtsError::InvalidInput {
                        detail: extract_error_detail(&body),
                    }),
                    code => Err(TtsError::Upstream {
                        status: code,
                        detail: extract_error_detail(&body),
                    }),
                };
            }

            if let Some(false) = content_type_is_audio {
                return Err(TtsError::InvalidResponse {
                    detail: "Content-Type không phải audio/*".to_string(),
                });
            }

            let bytes = response.bytes().await.map_err(|err| {
                if err.is_timeout() {
                    TtsError::Timeout {
                        timeout_ms: duration_ms(self.request_timeout),
                    }
                } else {
                    TtsError::InvalidResponse {
                        detail: err.to_string(),
                    }
                }
            })?;

            if bytes.is_empty() {
                return Err(TtsError::InvalidResponse {
                    detail: "dữ liệu MP3 trả về rỗng".to_string(),
                });
            }

            Ok(bytes.to_vec())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::extract_error_detail;

    #[test]
    fn extract_error_detail_parses_structured_json_and_falls_back_to_raw_text() {
        let json = r#"{"error":"rate_limited","detail":"Google Translate rate limit reached"}"#;
        assert_eq!(
            extract_error_detail(json),
            "Google Translate rate limit reached"
        );
        assert_eq!(
            extract_error_detail("  plain gateway error  "),
            "plain gateway error"
        );
        assert_eq!(extract_error_detail("   "), "empty error body");
    }
}
