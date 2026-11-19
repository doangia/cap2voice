# Test Log — Task 3: Temporary Local Python API Server

> Date: 2026-11-19 · Environment: Windows, Python 3.11.9
> Status: automated gates GREEN; live real-PC check GREEN with one environment limitation recorded below.
> Gate discipline: RED confirmed before each fix (see "Prove-it" entry).

## Files delivered

- `server/server.py` — FastAPI app (`/health`, `/translate`, `/tts`), typed Pydantic models,
  structured `ApiError` JSON, injectable `httpx` client + Edge-TTS factory,
  `python -m server.server` entry point binding `127.0.0.1:18765`.
- `server/requirements.txt` — pinned versions verified on this PC.
- `server/tests/test_server.py` — 15 contract tests with `httpx.MockTransport` and stub
  Edge-TTS factories (upstream boundary only; routing, validation, payload parsing,
  chunk filtering and error mapping execute for real).
- `server/__init__.py` — makes `server` importable from the repo root.
- `.gitignore` — added `.pytest_cache/` and `.mypy_cache/`.

## Automated checks — actual output

### `server/.venv/Scripts/python.exe -m pytest server/ -v`

```
collected 15 items

test_health_returns_exact_schema PASSED
test_translate_returns_vietnamese_and_requests_zh_to_vi PASSED
test_translate_rejects_whitespace_with_structured_400 PASSED
test_translate_maps_google_429_to_structured_rate_limit PASSED
test_translate_maps_network_failure_to_502 PASSED
test_translate_maps_timeout_to_502 PASSED
test_translate_maps_unexpected_payload_to_502 PASSED
test_tts_returns_concatenated_mp3_bytes PASSED
test_tts_accepts_female_voice_and_custom_rate PASSED
test_tts_rejects_empty_text_with_structured_400 PASSED
test_tts_rejects_unsupported_voice_with_structured_400 PASSED
test_tts_rejects_malformed_rate_with_structured_400 PASSED
test_tts_maps_edge_tts_failure_to_502 PASSED
test_tts_maps_raw_http_client_error_to_502 PASSED
test_tts_maps_no_audio_bytes_to_503 PASSED

15 passed in 0.59s
```

### `server/.venv/Scripts/python.exe -m mypy --strict server/server.py`

```
Success: no issues found in 1 source file
```

### Prove-it (bug found during review → RED → GREEN)

Edge-TTS may raise a raw `aiohttp.ClientError` (network failure) that is **not** a
subclass of `EdgeTTSException`, which produced an unhelpful plain 500. Added
`test_tts_maps_raw_http_client_error_to_502` first — it FAILED (response was 500),
then added `aiohttp.ClientError` to the caught exception tuple — test PASSED.

## Live real-PC check — actual output

Virtual environment created from the pinned requirements:
`python -m venv server/.venv` → `pip install -r server/requirements.txt` (all pins installed).

Server started:
`server/.venv/Scripts/python.exe -m uvicorn server:app --app-dir server --host 127.0.0.1 --port 18765`

Actual HTTP responses:

```
GET /health                     200  6.3 ms
  {"status":"ok","version":"0.1.0","translator":"google-free","tts":"edge-tts"}

POST /translate "夜深了，雨还在下。"
  429  {"error":"rate_limited","detail":"Google Translate rate limit reached"}

POST /translate "   "           400
  {"error":"invalid_input","detail":"text must not be empty"}

POST /tts vi-VN-NamMinhNeural   200  audio/mpeg  19008 bytes
  header: ff f3 64 c4 ...  (valid MPEG audio frame)
POST /tts vi-VN-HoaiMyNeural    200  audio/mpeg  18000 bytes
  header: ff f3 64 c4 ...  (valid MPEG audio frame)

POST /tts ""                    400
  {"error":"invalid_input","detail":"text must not be empty"}
POST /tts voice=en-US-AriaNeural 400
  {"error":"invalid_input","detail":"unsupported voice: en-US-AriaNeural"}
```

Network-failure mapping (separate instance with dead proxy `127.0.0.1:9`):

```
POST /translate (network down)  502
  {"error":"upstream_error","detail":"translation upstream unreachable"}
POST /tts (injected Edge-TTS WebSocketError)  502
  {"error":"upstream_error","detail":"Edge-TTS failed: simulated Edge-TTS offline"}
```

Server stopped cleanly; no listener remains on ports 18765 / 18766 / 18767.

## Environment limitation (honest report, not a code failure)

Google Translate's free endpoint returns **HTTP 429** from this machine's IP for all
tested hostnames (`translate.googleapis.com`, `translate.google.com`,
`translate.google.com.hk`), with and without a browser User-Agent. The server maps
it correctly to structured `429 rate_limited` — this is exactly the upstream
rate-limit path described in `docs/spec.md` §3.3 / AC-SRV-2, and the Rust client
already plans a 2 s backoff. Therefore a live **200 zh→vi** translation could not
be captured here; the 200 path is covered by an automated test that asserts the
outbound request uses `sl=zh-CN`, `tl=vi`, `q=<text>` and returns the parsed first
segment. Re-run the live `/translate` happy path on Danh's real PC or from a
different IP before Checkpoint 1 sign-off.

MP3 bytes were generated and saved under the temp folder; listening/playback
confirmation ("intelligible Vietnamese speech") still needs Danh's ears on the PC —
audio byte-level validation (non-empty, MPEG frame header, correct voice, correct
rate) is recorded above.
