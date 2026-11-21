"""Temporary local FastAPI server for cap2voice.

Exposes three strictly typed endpoints used by the Rust application:

* ``GET /health``  — readiness probe managed by ``ServerProcess``.
* ``POST /translate`` — offline Vietphrase dictionary translation (zh -> vi).
* ``POST /tts`` — Edge-TTS synthesis (vi text -> MP3 bytes).

Translation runs fully offline through ``server.vietphrase`` (a port of the
GPL-3.0 ``duongden/script-vietphrase-translator`` userscript; dictionaries are
fetched separately, see ``server/vietphrase/README.md``). It replaced the
Google Translate free endpoint on 2026-11-20 because Google returned HTTP 429
from this IP for ~2 days. When the dictionaries are missing, ``/translate``
answers 503 with the exact command that fetches them. Every upstream failure
is mapped to a structured JSON body ``{"error": ..., "detail": ...}``; nothing
is silently swallowed. This module is deliberately temporary and is replaced
by native Rust in V2.
"""

from __future__ import annotations

import asyncio
import re
from typing import Protocol

import aiohttp
from edge_tts import Communicate
from edge_tts.exceptions import EdgeTTSException
from fastapi import Depends, FastAPI, Request
from fastapi.responses import JSONResponse, Response
from pydantic import BaseModel, Field

APP_VERSION = "0.1.0"
BIND_HOST = "127.0.0.1"
DEFAULT_PORT = 18765

DEFAULT_VOICE = "vi-VN-NamMinhNeural"
SUPPORTED_VOICES = frozenset({DEFAULT_VOICE, "vi-VN-HoaiMyNeural"})
DEFAULT_RATE = "+0%"

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


def translator_unavailable(detail: str) -> ApiError:
    return ApiError(503, "translator_unavailable", detail)


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


class Translator(Protocol):
    """Synchronous Chinese-to-Vietnamese translator boundary."""

    def translate(self, text: str) -> str: ...


class CommunicateFactory(Protocol):
    """Callable boundary around ``edge_tts.Communicate`` for injection in tests."""

    def __call__(self, text: str, voice: str, *, rate: str) -> Communicate: ...


def get_communicate_factory() -> CommunicateFactory:
    return Communicate


def _load_vietphrase_translator() -> Translator:
    """Load the Vietphrase dictionaries and build the translator.

    This module is loaded two ways: as top-level ``server`` by uvicorn
    (``--app-dir server``) and as ``server.server`` by pytest from the repo
    root, so the package path is resolved from ``__package__`` at runtime.
    The dictionaries live in ``server/vietphrase/data`` and are not bundled
    with the repo (GPL-3.0 upstream data); ``load_default_dictionaries``
    raises ``RuntimeError`` with the exact fetch command when they are
    missing.
    """
    import importlib
    from typing import cast

    package = "server.vietphrase" if __package__ else "vietphrase"
    module = importlib.import_module(package)
    return cast(
        Translator, module.VietPhraseTranslator(module.load_default_dictionaries())
    )


_translator: Translator | None = None
_translator_lock = asyncio.Lock()


async def get_translator() -> Translator:
    """Return the shared offline translator, loading it once on first use.

    Dictionary loading takes ~1s, so it runs in a worker thread. When the
    dictionaries are missing, the endpoint answers 503 instead of falling
    back to any network translator.
    """
    global _translator
    if _translator is None:
        async with _translator_lock:
            if _translator is None:
                try:
                    _translator = await asyncio.to_thread(_load_vietphrase_translator)
                except RuntimeError as exc:
                    raise translator_unavailable(str(exc)) from exc
    return _translator


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
        "translator": "vietphrase",
        "tts": "edge-tts",
    }


@app.post("/translate", response_model=TranslateResponse)
async def translate(
    req: TranslateRequest,
    translator: Translator = Depends(get_translator),
) -> TranslateResponse:
    text = req.text.strip()
    if not text:
        raise invalid_input("text must not be empty")
    translated = await asyncio.to_thread(translator.translate, text)
    return TranslateResponse(text=translated)


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
