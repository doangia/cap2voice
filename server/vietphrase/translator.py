"""Vietphrase dictionary translator ported from ``vietphrase.user.js``.

This module contains the DOM-free translation pipeline: dictionary parsing,
punctuation normalisation, name/rule segmentation, longest VP matching,
placeholder resolution, casing and spacing.  The browser-only parts of the
userscript (MutationObserver, DOM rewriting, clipboard, dictionary manager UI)
are deliberately excluded; callers feed text in and get text plus a token map
out.
"""

from __future__ import annotations

import re
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path
from typing import TypedDict
from urllib.request import Request, urlopen

from .rule_engine import RuleEngine, create_rule_engine

CHINESE_RE = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\u3007]")
DICH_LIEU_SET = frozenset({"的", "了", "着", "著"})

DEFAULT_DICT_DIR = Path(__file__).resolve().parent / "data"

DEFAULT_DICT_URLS = {
    "pa": "https://raw.githubusercontent.com/duongden/script-vietphrase-translator/refs/heads/main/ChinesePhienAmWords.txt",
    "vp": "https://raw.githubusercontent.com/duongden/script-vietphrase-translator/refs/heads/main/Vietphrase.txt",
    "names": "https://raw.githubusercontent.com/duongden/script-vietphrase-translator/refs/heads/main/Names.txt",
    "pronouns": "https://raw.githubusercontent.com/duongden/script-vietphrase-translator/refs/heads/main/Pronouns.txt",
    "rules": "https://raw.githubusercontent.com/duongden/script-vietphrase-translator/refs/heads/main/rule.txt",
}
_DICT_FILENAMES = {
    "pa": "ChinesePhienAmWords.txt",
    "vp": "Vietphrase.txt",
    "names": "Names.txt",
    "pronouns": "Pronouns.txt",
    "rules": "rule.txt",
}

PUNCT_MAP: tuple[tuple[str, str], ...] = (
    ("。》", ".\x03"),
    ("。』", ".\x03"),
    ("。」", ".\x03"),
    ("？》", "?\x03"),
    ("！》", "!\x03"),
    ("《", "\x02"),
    ("》", "\x03"),
    ("〈", "\x02"),
    ("〉", "\x03"),
    ("「", "\x02"),
    ("」", "\x03"),
    ("『", "\x02"),
    ("』", "\x03"),
    ("“", "\x02"),
    ("”", "\x03"),
    ("‘", "\x04"),
    ("’", "\x05"),
    ("【", "["),
    ("】", "]"),
    ("〔", "["),
    ("〕", "]"),
    ("〖", "["),
    ("〗", "]"),
    ("（", "("),
    ("）", ")"),
    ("｛", "{"),
    ("｝", "}"),
    ("。", "."),
    ("！", "!"),
    ("？", "?"),
    ("；", ";"),
    ("：", ":"),
    ("，", ","),
    ("、", ", "),
    ("……", "..."),
    ("…", "..."),
    ("——", "—"),
    ("—", "—"),
    ("－", "-"),
    ("～", "~"),
    ("•", "·"),
    ("　", " "),
    ("／", "/"),
    ("＼", "\\"),
    ("！", "!"),
    ("＂", '"'),
    ("＃", "#"),
    ("＄", "$"),
    ("％", "%"),
    ("＆", "&"),
    ("＇", "'"),
    ("＊", "*"),
    ("＋", "+"),
    ("＜", "<"),
    ("＝", "="),
    ("＞", ">"),
    ("＠", "@"),
    ("［", "["),
    ("］", "]"),
    ("＾", "^"),
    ("＿", "_"),
    ("｀", "`"),
    ("｜", "|"),
)

VIET_RE = re.compile(r"[a-zA-ZÀ-ỹ]")
CLOSE_RE = re.compile(r"^[\x03,.:;!?)»\]]")
OPEN_RE = re.compile(r"[\x02([]\s*$")
_ASCII_UPPER_RE = re.compile(r"^[A-ZÀ-ỸĐ]")
_SPACES_RE = re.compile(r"\s{2,}")


@dataclass
class Settings:
    """Translation options. DOM-only fields are kept for userscript parity."""

    ngoac: bool = False
    motnghia: bool = True
    daucach: str = "/"
    dichlieu: bool = True
    rule_enabled: bool = True
    heightauto: bool = True
    scaleauto: bool = True
    delay_mutation: int = 200
    delay_trans: int = 120


@dataclass
class DictionarySet:
    """Raw dictionary text for one translator instance."""

    pa: dict[str, str] = field(default_factory=dict)
    vp: dict[str, str] = field(default_factory=dict)
    names: dict[str, str] = field(default_factory=dict)
    pronouns: dict[str, str] = field(default_factory=dict)
    rules: str = ""


@dataclass
class TokenMapEntry:
    """Alignment between a source (Han) span and its translated (Viet) span."""

    han: str
    viet: str
    start: int
    end: int


@dataclass
class TranslationResult:
    result: str
    token_map: list[TokenMapEntry]


class Pair(TypedDict):
    han: str
    viet: str
    type: str


class Segment(TypedDict):
    text: str
    han: str
    is_name: bool
    is_rule: bool


def fix_vietnamese(text: object) -> str:
    """Mirror JS ``(text ?? '').toString().normalize('NFC')``."""
    return unicodedata.normalize("NFC", "" if text is None else str(text))


def normalize_dict_object(data: dict[str, str]) -> dict[str, str]:
    return {fix_vietnamese(key): fix_vietnamese(value) for key, value in data.items()}


def parse_dictionary(text: str, mode: str = "") -> dict[str, str]:
    """Parse an ``Hán=Việt`` TXT file; ``mode='PA'`` allows single Han chars only."""
    out: dict[str, str] = {}
    # Normalise per field, not on the whole file: unicodedata.normalize on a
    # 24 MB blob is very slow on CPython, while per-entry calls produce the
    # identical result (NFC is stable under substring slicing).
    for raw in re.split(r"\r?\n", text):
        line = raw.strip()
        if not line or line.startswith("//") or line.startswith("#") or line.startswith("="):
            continue
        eq = line.find("=")
        if eq < 1:
            continue
        key = fix_vietnamese(line[:eq]).strip()
        value = fix_vietnamese(line[eq + 1 :]).strip()
        if not key or not value:
            continue
        if mode == "PA":
            chars = list(key)
            if len(chars) != 1 or not all(CHINESE_RE.fullmatch(char) for char in chars):
                continue
        out[key] = value
    return out


def load_dictionary_set(directory: str | Path) -> DictionarySet:
    """Load the five userscript dictionary files from ``directory``."""
    root = Path(directory)

    def read(name: str) -> str:
        return (root / name).read_text(encoding="utf-8")

    return DictionarySet(
        pa=parse_dictionary(read(_DICT_FILENAMES["pa"]), "PA"),
        vp=parse_dictionary(read(_DICT_FILENAMES["vp"])),
        names=parse_dictionary(read(_DICT_FILENAMES["names"])),
        pronouns=parse_dictionary(read(_DICT_FILENAMES["pronouns"])),
        rules=fix_vietnamese(read(_DICT_FILENAMES["rules"])),
    )


def load_default_dictionaries() -> DictionarySet:
    """Load the dictionaries from ``server/vietphrase/data`` (offline).

    The data directory is not bundled with the repo (GPL-3.0 upstream data,
    see ``server/vietphrase/README.md``). When it is missing, fail loudly
    with the exact command that fetches it instead of a bare
    ``FileNotFoundError``.
    """
    missing = [
        name
        for name in _DICT_FILENAMES.values()
        if not (DEFAULT_DICT_DIR / name).is_file()
    ]
    if missing:
        raise RuntimeError(
            "Vietphrase dictionaries are missing from "
            f"{DEFAULT_DICT_DIR} ({', '.join(missing)}). "
            "They are not bundled with the repo; download them with "
            "'python -m server.vietphrase --fetch --text \"...\"' "
            "or copy the five TXT files from "
            "https://github.com/duongden/script-vietphrase-translator "
            "into that directory."
        )
    return load_dictionary_set(DEFAULT_DICT_DIR)


def fetch_default_dictionaries(
    cache_dir: str | Path | None = None,
    timeout: float = 30.0,
    save_dir: str | Path | None = None,
) -> DictionarySet:
    """Fetch the canonical dictionaries from the userscript's upstream repo.

    Network failures raise ``RuntimeError`` with the failing URL included —
    the app must report failures instead of silently skipping translation.
    When ``cache_dir`` is given, downloaded files are reused on later calls.
    When ``save_dir`` is given, the downloaded files are also written there
    (used by the CLI to provision ``server/vietphrase/data`` on first run).
    """

    def fetch(key: str) -> str:
        filename = _DICT_FILENAMES[key]
        cached = Path(cache_dir) / filename if cache_dir else None
        if cached is not None and cached.is_file():
            text = cached.read_text(encoding="utf-8")
        else:
            url = DEFAULT_DICT_URLS[key]
            try:
                with urlopen(Request(url, headers={"User-Agent": "cap2voice-vietphrase"}), timeout=timeout) as response:
                    text = response.read().decode("utf-8")
            except Exception as exc:
                raise RuntimeError(f"Cannot fetch dictionary {key}: {url}: {exc}") from exc
            if cached is not None:
                cached.parent.mkdir(parents=True, exist_ok=True)
                cached.write_text(text, encoding="utf-8")
        if save_dir is not None:
            target = Path(save_dir) / filename
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8")
        return text

    return DictionarySet(
        pa=parse_dictionary(fetch("pa"), "PA"),
        vp=parse_dictionary(fetch("vp")),
        names=parse_dictionary(fetch("names")),
        pronouns=parse_dictionary(fetch("pronouns")),
        rules=fix_vietnamese(fetch("rules")),
    )


def has_han_char(text: str) -> bool:
    return bool(CHINESE_RE.search(str(text or "")))


def is_han_char(char: str) -> bool:
    return bool(CHINESE_RE.fullmatch(char))


def take_non_han_run(text: str, start: int) -> str:
    end = start
    while end < len(text) and not is_han_char(text[end]):
        end += 1
    return text[start:end]


def normalize_punct(text: str) -> str:
    for source, target in PUNCT_MAP:
        if source in text:
            text = text.replace(source, target)
    return text


def resolve_placeholders(text: str) -> str:
    return (
        text.replace("\x02", '"')
        .replace("\x03", '"')
        .replace("\x04", "‘")
        .replace("\x05", "’")
    )


def join_translated_tokens(tokens: list[str]) -> str:
    result = ""
    for token in tokens:
        if not token:
            continue
        if not result:
            result = token
            continue
        last = result[-1]
        first = token[0]
        if CLOSE_RE.match(first):
            result += token
        elif OPEN_RE.search(last):
            result += token
        elif last == " ":
            result += token
        elif VIET_RE.search(last) or VIET_RE.search(first):
            result += " " + token
        elif last in ("\x03", "\x05"):
            result += " " + token
        else:
            result += token
    return result


def normalize_token_case(pairs: list[Pair]) -> None:
    context = ""
    for pair in pairs:
        value = str(pair["viet"] or "")
        starts_sentence = (
            not context.strip()
            or bool(re.search(r"[.!?]\s*$", context))
            or bool(re.search(r"\x02\s*$", context))
        )
        if not starts_sentence and pair["type"] != "name" and _ASCII_UPPER_RE.match(value):
            value = value[0].lower() + value[1:]
            pair["viet"] = value
        context = join_translated_tokens([context, value])


def post_process_translated_text(text: str) -> str:
    text = _SPACES_RE.sub(" ", text)
    text = re.sub(r"\s+([\x03,.:;!?)»\]])", r"\1", text)
    text = re.sub(r"([\x02([])\s+", r"\1", text)
    text = re.sub(r"(\x03)([a-zA-ZÀ-ỹ\x02([])", r"\1 \2", text)
    text = re.sub(r"([a-zA-ZÀ-ỹ\x03\])])(\x02)", r"\1 \2", text)
    text = re.sub(r"([:;!?,])([a-zA-ZÀ-ỹ\x02])", r"\1 \2", text)
    text = re.sub(r"([a-zA-ZÀ-ỹ])([([])", r"\1 \2", text)
    text = re.sub(r"(^|[^0-9]),(?=[0-9])", r"\1, ", text)
    return text.strip()


_LOWER_VI = (
    "a-zàáâãèéêìíòóôõùúýăđơưạảấầẩẫậắằẳẵặẹẻẽếềểễệỉịọỏốồổỗộớờởỡợụủứừửữựỳỷỹ"
)


def auto_capitalize(text: str) -> str:
    if not text or not text.strip():
        return text
    trimmed = text.lstrip()
    if trimmed:
        text = trimmed[0].upper() + trimmed[1:]
    text = re.sub(rf"([.!?])(\s*)([{_LOWER_VI}])", lambda m: m.group(1) + m.group(2) + m.group(3).upper(), text)
    text = re.sub(rf"(:\s+\x02)([{_LOWER_VI}])", lambda m: m.group(1) + m.group(2).upper(), text)
    text = re.sub(rf"((?:^|\s)\x02)([{_LOWER_VI}])", lambda m: m.group(1) + m.group(2).upper(), text)
    return text


class VietPhraseTranslator:
    """Dictionary-based Chinese→Vietnamese translator.

    ``translate_with_map`` mirrors ``translateTextWithMap`` from the userscript;
    ``translate`` returns only the final Vietnamese string.  The token map is
    useful for the overlay: each entry pairs the original Han span with the
    Vietnamese span it became after post-processing.
    """

    def __init__(self, dictionaries: DictionarySet, settings: Settings | None = None) -> None:
        self.settings = settings or Settings()
        self.dict_pa = normalize_dict_object(dictionaries.pa)
        self.dict_vp = normalize_dict_object(dictionaries.vp)
        self.dict_names = normalize_dict_object(dictionaries.names)
        self.dict_pronouns = normalize_dict_object(dictionaries.pronouns)
        self.rule_text = fix_vietnamese(dictionaries.rules)

        self.dict_vp_keys = sorted(self.dict_vp, key=lambda key: (-len(key), key))
        self.dict_names_keys = sorted(self.dict_names, key=lambda key: (-len(key), key))
        self.dict_vp_max_len = len(self.dict_vp_keys[0]) if self.dict_vp_keys else 1
        self.dict_names_max_len = len(self.dict_names_keys[0]) if self.dict_names_keys else 0

        self.rule_engine: RuleEngine | None = create_rule_engine(
            self.rule_text,
            {
                "pa": self.dict_pa,
                "vp": self.dict_vp,
                "names": self.dict_names,
                "pronouns": self.dict_pronouns,
            },
            {"separator": self.settings.daucach, "vpMaxLen": self.dict_vp_max_len},
        )

    def translate(self, text: str) -> str:
        return self.translate_with_map(text).result

    def translate_with_map(self, text: str) -> TranslationResult:
        text = fix_vietnamese(text)
        if not text or not text.strip() or not has_han_char(text):
            return TranslationResult(result=text, token_map=[])

        ngoac = self.settings.ngoac
        motnghia = self.settings.motnghia
        daucach = self.settings.daucach
        dichlieu = self.settings.dichlieu
        text = normalize_punct(text)

        rule_segments: list[Segment] = []
        if (
            self.settings.rule_enabled
            and self.rule_engine is not None
            and self.rule_engine.rules
        ):
            plain_start = 0
            cursor = 0
            while cursor < len(text):
                hit = self.rule_engine.match_at(text, cursor)
                if not hit:
                    cursor += 1
                    continue
                if plain_start < cursor:
                    rule_segments.append(self._segment(text[plain_start:cursor]))
                rule_segments.append(
                    {
                        "text": hit["translation"],
                        "han": hit["source"],
                        "is_name": False,
                        "is_rule": True,
                    }
                )
                cursor = hit["end"]
                plain_start = cursor
            if plain_start < len(text):
                rule_segments.append(self._segment(text[plain_start:]))
        else:
            rule_segments.append(self._segment(text))

        segments: list[Segment] = []
        for segment in rule_segments:
            if segment["is_rule"]:
                segments.append(segment)
            else:
                segments.extend(self._take_name_segments(segment["text"]))

        pairs: list[Pair] = []
        max_len = self.dict_vp_max_len
        for segment in segments:
            if segment["is_rule"]:
                pairs.append({"han": segment["han"], "viet": segment["text"], "type": ""})
                continue
            if segment["is_name"]:
                pairs.append(
                    {"han": segment["han"], "viet": segment["text"], "type": "name"}
                )
                continue
            source = segment["text"]
            i = 0
            while i < len(source):
                matched = False
                remaining = len(source) - i
                for length in range(min(max_len, remaining), 0, -1):
                    sub = source[i : i + length]
                    vp = self.dict_vp.get(sub)
                    if vp is not None:
                        viet = vp.split(daucach)[0].strip() if motnghia else vp.strip()
                        if ngoac:
                            viet = "[" + viet + "]"
                        pairs.append({"han": sub, "viet": viet, "type": ""})
                        i += length
                        matched = True
                        break
                if matched:
                    continue
                char = source[i]
                if not is_han_char(char):
                    raw = take_non_han_run(source, i)
                    if raw:
                        pairs.append({"han": raw, "viet": raw, "type": ""})
                        i += len(raw) or 1
                        continue
                if dichlieu and char in DICH_LIEU_SET:
                    i += 1
                    continue
                pairs.append({"han": char, "viet": self.dict_pa.get(char, char), "type": ""})
                i += 1

        normalize_token_case(pairs)
        result = post_process_translated_text(join_translated_tokens([pair["viet"] for pair in pairs]))

        token_map: list[TokenMapEntry] = []
        search_from = 0
        for pair in pairs:
            if not pair["viet"]:
                continue
            start = result.find(pair["viet"], search_from)
            if start < 0:
                start = result.find(pair["viet"])
            if start < 0:
                continue
            end = start + len(pair["viet"])
            token_map.append(TokenMapEntry(han=pair["han"], viet=pair["viet"], start=start, end=end))
            search_from = end

        result = fix_vietnamese(resolve_placeholders(auto_capitalize(result)))
        for token in token_map:
            token.viet = result[token.start : token.end]
        return TranslationResult(result=result, token_map=token_map)

    @staticmethod
    def _segment(text: str) -> Segment:
        return {"text": text, "han": "", "is_name": False, "is_rule": False}

    def _take_name_segments(self, text: str) -> list[Segment]:
        if not self.dict_names_max_len:
            return [self._segment(text)]
        segments: list[Segment] = []
        plain_start = 0
        i = 0
        while i < len(text):
            name = ""
            remaining = len(text) - i
            for length in range(min(self.dict_names_max_len, remaining), 0, -1):
                candidate = text[i : i + length]
                if candidate in self.dict_names:
                    name = candidate
                    break
            if not name:
                i += 1
                continue

            vp_length = 0
            for length in range(min(self.dict_vp_max_len, remaining), len(name) - 1, -1):
                if text[i : i + length] in self.dict_vp:
                    vp_length = length
                    break
            if vp_length >= len(name):
                i += 1
                continue

            if plain_start < i:
                segments.append(self._segment(text[plain_start:i]))
            name_value = self.dict_names[name]
            name_value = (
                name_value.split(self.settings.daucach)[0].strip()
                if self.settings.motnghia
                else name_value.strip()
            )
            if self.settings.ngoac:
                name_value = "[" + name_value + "]"
            segments.append(
                {"text": name_value, "han": name, "is_name": True, "is_rule": False}
            )
            i += len(name)
            plain_start = i

        if plain_start < len(text):
            segments.append(self._segment(text[plain_start:]))
        return segments


__all__ = [
    "DictionarySet",
    "Settings",
    "TokenMapEntry",
    "TranslationResult",
    "VietPhraseTranslator",
    "DEFAULT_DICT_DIR",
    "fetch_default_dictionaries",
    "load_default_dictionaries",
    "load_dictionary_set",
    "parse_dictionary",
]
