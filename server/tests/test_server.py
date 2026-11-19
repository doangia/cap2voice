"""Contract tests for the temporary cap2voice FastAPI server (AC-SRV-1..3).

Upstream boundaries are simulated with ``httpx.MockTransport`` and a stub
Edge-TTS ``Communicate`` factory, so the real request construction, payload
parsing, validation, chunk filtering and error mapping still execute.
"""

from __future__ import annotations

from collections.abc import AsyncIterator, Callable, Iterator

import aiohttp
import edge_tts
import httpx
import pytest
from edge_tts.typing import TTSChunk
from fastapi.testclient import TestClient

from server.server import app, get_communicate_factory, get_http_client

ZH_LINE = "夜深了，雨还在下。"
VI_LINE = "Đêm đã khuya, mưa vẫn còn rơi."
GOOGLE_PAYLOAD: object = [[[VI_LINE, ZH_LINE, None, None, 10]], None, "zh-CN"]


@pytest.fixture
def client() -> Iterator[TestClient]:
    with TestClient(app) as test_client:
        yield test_client
    app.dependency_overrides.clear()


def override_http(handler: Callable[[httpx.Request], httpx.Response]) -> None:
    async def fake_http_client() -> AsyncIterator[httpx.AsyncClient]:
        transport = httpx.MockTransport(handler)
        async with httpx.AsyncClient(transport=transport) as http_client:
            yield http_client

    app.dependency_overrides[get_http_client] = fake_http_client


def override_tts(factory: Callable[..., object]) -> None:
    app.dependency_overrides[get_communicate_factory] = lambda: factory


class StubCommunicate:
    """Streams boundary chunks plus two audio chunks, like real Edge-TTS."""

    def __init__(self, text: str, voice: str, *, rate: str) -> None:
        self.text = text
        self.voice = voice
        self.rate = rate

    async def stream(self) -> AsyncIterator[TTSChunk]:
        yield {
            "type": "SentenceBoundary",
            "offset": 0,
            "duration": 100,
            "text": self.text,
        }
        yield {"type": "audio", "data": b"ID3\x03"}
        yield {"type": "audio", "data": b"\xff\xfb\x90\x00"}


class FailingCommunicate:
    async def stream(self) -> AsyncIterator[TTSChunk]:
        raise edge_tts.exceptions.WebSocketError("edge-tts upstream unreachable")
        yield {"type": "audio", "data": b""}  # unreachable, keeps this an async generator


class SilentCommunicate:
    async def stream(self) -> AsyncIterator[TTSChunk]:
        yield {"type": "WordBoundary", "offset": 0, "duration": 10, "text": "x"}


class TransportFailingCommunicate:
    async def stream(self) -> AsyncIterator[TTSChunk]:
        raise aiohttp.ClientError("cannot reach Edge-TTS websocket")
        yield {"type": "audio", "data": b""}  # unreachable, keeps this an async generator


def stub_factory() -> tuple[Callable[..., StubCommunicate], list[StubCommunicate]]:
    created: list[StubCommunicate] = []

    def factory(text: str, voice: str, *, rate: str) -> StubCommunicate:
        communicate = StubCommunicate(text, voice, rate=rate)
        created.append(communicate)
        return communicate

    return factory, created


def google_ok(request: httpx.Request) -> httpx.Response:
    return httpx.Response(200, json=GOOGLE_PAYLOAD)


# --- AC-SRV-1: GET /health -------------------------------------------------


def test_health_returns_exact_schema(client: TestClient) -> None:
    response = client.get("/health")

    assert response.status_code == 200
    assert response.json() == {
        "status": "ok",
        "version": "0.1.0",
        "translator": "google-free",
        "tts": "edge-tts",
    }


# --- AC-SRV-2: POST /translate ---------------------------------------------


def test_translate_returns_vietnamese_and_requests_zh_to_vi(
    client: TestClient,
) -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return google_ok(request)

    override_http(handler)

    response = client.post("/translate", json={"text": ZH_LINE})

    assert response.status_code == 200
    assert response.json() == {"text": VI_LINE}
    assert len(seen) == 1
    assert seen[0].url.params["sl"] == "zh-CN"
    assert seen[0].url.params["tl"] == "vi"
    assert seen[0].url.params["q"] == ZH_LINE


def test_translate_rejects_whitespace_with_structured_400(
    client: TestClient,
) -> None:
    override_http(google_ok)

    response = client.post("/translate", json={"text": "   \t  "})

    assert response.status_code == 400
    assert response.json() == {
        "error": "invalid_input",
        "detail": "text must not be empty",
    }


def test_translate_maps_google_429_to_structured_rate_limit(
    client: TestClient,
) -> None:
    override_http(lambda request: httpx.Response(429, json={"error": "quota"}))

    response = client.post("/translate", json={"text": ZH_LINE})

    assert response.status_code == 429
    assert response.json() == {
        "error": "rate_limited",
        "detail": "Google Translate rate limit reached",
    }


def test_translate_maps_network_failure_to_502(client: TestClient) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("network down", request=request)

    override_http(handler)

    response = client.post("/translate", json={"text": ZH_LINE})

    assert response.status_code == 502
    assert response.json() == {
        "error": "upstream_error",
        "detail": "translation upstream unreachable",
    }


def test_translate_maps_timeout_to_502(client: TestClient) -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ReadTimeout("too slow", request=request)

    override_http(handler)

    response = client.post("/translate", json={"text": ZH_LINE})

    assert response.status_code == 502
    assert response.json() == {
        "error": "upstream_error",
        "detail": "translation upstream timed out",
    }


def test_translate_maps_unexpected_payload_to_502(client: TestClient) -> None:
    override_http(lambda request: httpx.Response(200, json=[[]]))

    response = client.post("/translate", json={"text": ZH_LINE})

    assert response.status_code == 502
    assert response.json() == {
        "error": "upstream_error",
        "detail": "translation upstream returned an unexpected payload",
    }


# --- AC-SRV-3: POST /tts ----------------------------------------------------


def test_tts_returns_concatenated_mp3_bytes(client: TestClient) -> None:
    factory, created = stub_factory()
    override_tts(factory)

    response = client.post("/tts", json={"text": VI_LINE})

    assert response.status_code == 200
    assert response.headers["content-type"].startswith("audio/mpeg")
    assert response.content == b"ID3\x03\xff\xfb\x90\x00"
    assert len(created) == 1
    assert created[0].text == VI_LINE
    assert created[0].voice == "vi-VN-NamMinhNeural"
    assert created[0].rate == "+0%"


def test_tts_accepts_female_voice_and_custom_rate(client: TestClient) -> None:
    factory, created = stub_factory()
    override_tts(factory)

    response = client.post(
        "/tts",
        json={"text": VI_LINE, "voice": "vi-VN-HoaiMyNeural", "rate": "+10%"},
    )

    assert response.status_code == 200
    assert len(created) == 1
    assert created[0].voice == "vi-VN-HoaiMyNeural"
    assert created[0].rate == "+10%"


def test_tts_rejects_empty_text_with_structured_400(client: TestClient) -> None:
    factory, created = stub_factory()
    override_tts(factory)

    response = client.post("/tts", json={"text": "  "})

    assert response.status_code == 400
    assert response.json() == {
        "error": "invalid_input",
        "detail": "text must not be empty",
    }
    assert created == []


def test_tts_rejects_unsupported_voice_with_structured_400(
    client: TestClient,
) -> None:
    factory, created = stub_factory()
    override_tts(factory)

    response = client.post("/tts", json={"text": VI_LINE, "voice": "en-US-AriaNeural"})

    assert response.status_code == 400
    assert response.json() == {
        "error": "invalid_input",
        "detail": "unsupported voice: en-US-AriaNeural",
    }
    assert created == []


def test_tts_rejects_malformed_rate_with_structured_400(client: TestClient) -> None:
    factory, created = stub_factory()
    override_tts(factory)

    response = client.post("/tts", json={"text": VI_LINE, "rate": "fast"})

    assert response.status_code == 400
    assert response.json() == {
        "error": "invalid_input",
        "detail": "invalid rate format: fast",
    }
    assert created == []


def test_tts_maps_edge_tts_failure_to_502(client: TestClient) -> None:
    def failing_factory(text: str, voice: str, *, rate: str) -> FailingCommunicate:
        return FailingCommunicate()

    override_tts(failing_factory)

    response = client.post("/tts", json={"text": VI_LINE})

    assert response.status_code == 502
    assert response.json() == {
        "error": "upstream_error",
        "detail": "Edge-TTS failed: edge-tts upstream unreachable",
    }


def test_tts_maps_raw_http_client_error_to_502(client: TestClient) -> None:
    def transport_failing_factory(
        text: str, voice: str, *, rate: str
    ) -> TransportFailingCommunicate:
        return TransportFailingCommunicate()

    override_tts(transport_failing_factory)

    response = client.post("/tts", json={"text": VI_LINE})

    assert response.status_code == 502
    assert response.json()["error"] == "upstream_error"


def test_tts_maps_no_audio_bytes_to_503(client: TestClient) -> None:
    def silent_factory(text: str, voice: str, *, rate: str) -> SilentCommunicate:
        return SilentCommunicate()

    override_tts(silent_factory)

    response = client.post("/tts", json={"text": VI_LINE})

    assert response.status_code == 503
    assert response.json() == {
        "error": "upstream_error",
        "detail": "Edge-TTS returned no audio data",
    }
