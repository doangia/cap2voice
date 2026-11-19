"""Temporary local FastAPI server for cap2voice.

Exposes three strictly typed endpoints used by the Rust application:

* ``GET /health``  — readiness probe managed by ``ServerProcess``.
* ``POST /translate`` — Google Translate free endpoint (zh-CN -> vi).
* ``POST /tts`` — Edge-TTS synthesis (vi text -> MP3 bytes).

Every upstream failure is mapped to a structured JSON body
``{"error": ..., "detail": ...}``; nothing is silently swallowed. This module
is deliberately temporary and is replaced by native Rust in V2.
"""

from __future__ import annotations

import re
from collections.abc import AsyncIterator
from typing import Protocol

import aiohttp
import httpx
from edge_tts import Communicate
from edge_tts.exceptions import EdgeTTSException
from edge_tts.typing import TTSChunk
from fastapi import Depends, FastAPI, Request
from fastapi.responses import JSONResponse, Response
from pydantic import BaseModel, Field

APP_VERSION = "0.1.0"
BIND_HOST = "127.0.0.1"
DEFAULT_PORT = 18765

DEFAULT_VOICE = "vi-VN-NamMinhNeural"
SUPPORTED_VOICES = frozenset({DEFAULT_VOICE, "vi-VN-HoaiMyNeural"})
DEFAULT_RATE = "+0%"

GOOGLE_TRANSLATE_URL = "https://translate.googleapis.com/translate_a/single"
HTTP_CLIENT_TIMEOUT_SECONDS = 8.0

_RATE_PATTERN = re.compile(r"[+-]\d{1,3}%")

app = FastAPI(title="cap2voice-local-api", version=APP_VERSION)


class ApiError(Exception):
    """Application error rendered as a flat, structured JSON body."""

    def __init__(self, status_code: int, code: str, detail: str) -> None:
        super().__init__(detail)
        self.status_code = status_code
        self.code = code
        self.detail = detail


@app.exception_handler(ApiError)
async def api_error_handler(_request: Request, exc: ApiError) -> JSONResponse:
    return JSONResponse(
        status_code=exc.status_code,
        content={"error": exc.code, "detail": exc.detail},
    )


def invalid_input(detail: str) -> ApiError:
    return ApiError(400, "invalid_input", detail)


def rate_limited(detail: str) -> ApiError:
    return ApiError(429, "rate_limited", detail)


def upstream_error(detail: str, status_code: int = 502) -> ApiError:
    return ApiError(status_code, "upstream_error", detail)


class TranslateRequest(BaseModel):
    text: str = Field(max_length=2000)


class TranslateResponse(BaseModel):
    text: str


class TtsRequest(BaseModel):
    text: str = Field(max_length=2000)
    voice: str = DEFAULT_VOICE
    rate: str = DEFAULT_RATE


class CommunicateFactory(Protocol):
    """Callable boundary around ``edge_tts.Communicate`` for injection in tests."""

    def __call__(self, text: str, voice: str, *, rate: str) -> Communicate: ...


async def get_http_client() -> AsyncIterator[httpx.AsyncClient]:
    async with httpx.AsyncClient(timeout=HTTP_CLIENT_TIMEOUT_SECONDS) as client:
        yield client


def get_communicate_factory() -> CommunicateFactory:
    return Communicate


def extract_translation(payload: object) -> str:
    """Read the first translated segment from Google's nested list response."""
    if not isinstance(payload, list) or not payload:
        return ""

    first_group: object = payload[0]
    if not isinstance(first_group, list) or not first_group:
        return ""

    first_segment: object = first_group[0]
    if not isinstance(first_segment, list) or not first_segment:
        return ""

    translated: object = first_segment[0]
    return translated if isinstance(translated, str) else ""


async def fetch_google_translation(client: httpx.AsyncClient, text: str) -> str:
    """Call Google's free endpoint, mapping every failure mode to ``ApiError``."""
    try:
        response = await client.get(
            GOOGLE_TRANSLATE_URL,
            params={
                "client": "gtx",
                "sl": "zh-CN",
                "tl": "vi",
                "dt": "t",
                "q": text,
            },
        )
    except httpx.TimeoutException as exc:
        raise upstream_error("translation upstream timed out") from exc
    except httpx.RequestError as exc:
        raise upstream_error("translation upstream unreachable") from exc

    if response.status_code == 429:
        raise rate_limited("Google Translate rate limit reached")
    if response.status_code != 200:
        raise upstream_error(f"translation upstream returned HTTP {response.status_code}")

    try:
        payload: object = response.json()
    except ValueError as exc:
        raise upstream_error("translation upstream returned invalid JSON") from exc

    translated = extract_translation(payload)
    if not translated:
        raise upstream_error("translation upstream returned an unexpected payload")
    return translated


def validate_rate(rate: str) -> None:
    if _RATE_PATTERN.fullmatch(rate) is None or abs(int(rate[:-1])) > 100:
        raise invalid_input(f"invalid rate format: {rate}")


async def synthesize_speech(
    factory: CommunicateFactory, text: str, voice: str, rate: str
) -> bytes:
    """Stream Edge-TTS audio chunks into one MP3 payload."""
    audio_chunks: list[bytes] = []
    try:
        communicate = factory(text, voice, rate=rate)
        async for chunk in communicate.stream():
            if chunk["type"] == "audio":
                data: bytes | None = chunk.get("data")
                if data is not None:
                    audio_chunks.append(data)
    except (EdgeTTSException, aiohttp.ClientError, OSError, TimeoutError) as exc:
        raise upstream_error(f"Edge-TTS failed: {exc}") from exc

    mp3 = b"".join(audio_chunks)
    if not mp3:
        raise upstream_error("Edge-TTS returned no audio data", 503)
    return mp3


@app.get("/health")
async def health() -> dict[str, str]:
    return {
        "status": "ok",
        "version": APP_VERSION,
        "translator": "google-free",
        "tts": "edge-tts",
    }


@app.post("/translate", response_model=TranslateResponse)
async def translate(
    req: TranslateRequest,
    client: httpx.AsyncClient = Depends(get_http_client),
) -> TranslateResponse:
    text = req.text.strip()
    if not text:
        raise invalid_input("text must not be empty")
    return TranslateResponse(text=await fetch_google_translation(client, text))


@app.post("/tts", response_class=Response)
async def tts(
    req: TtsRequest,
    factory: CommunicateFactory = Depends(get_communicate_factory),
) -> Response:
    text = req.text.strip()
    if not text:
        raise invalid_input("text must not be empty")
    if req.voice not in SUPPORTED_VOICES:
        raise invalid_input(f"unsupported voice: {req.voice}")
    validate_rate(req.rate)

    mp3 = await synthesize_speech(factory, text, req.voice, req.rate)
    return Response(content=mp3, media_type="audio/mpeg")


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(app, host=BIND_HOST, port=DEFAULT_PORT)
