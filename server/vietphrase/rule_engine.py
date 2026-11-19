"""QuickTranslate-compatible rule engine ported from ``vietphrase.user.js``.

The userscript exposes this as ``VPRuleEngine``.  Only the pure, DOM-free part
is ported here: rule parsing, number conversion, dictionary lookups and
``match_at``.  All semantics (capture sources, specificity ordering, number
boundary guards, VP-overwrite protection) mirror the JavaScript line by line
so the two implementations produce identical translations.
"""

from __future__ import annotations

import re
from typing import Literal, Mapping, TypedDict, Union

NUM_DIGITS = {
    "零": 0,
    "〇": 0,
    "○": 0,
    "一": 1,
    "二": 2,
    "两": 2,
    "兩": 2,
    "三": 3,
    "四": 4,
    "五": 5,
    "六": 6,
    "七": 7,
    "八": 8,
    "九": 9,
}
SMALL_UNITS = {"十": 10, "百": 100, "千": 1000}
LARGE_UNITS = {"万": 10000, "萬": 10000, "亿": 100000000, "億": 100000000}

NUMERIC_CHARS = "0-9零〇○一二两兩三四五六七八九十百千万萬亿億"
NUMERIC_RE = re.compile(rf"[{NUMERIC_CHARS}]")
YEAR_RE = re.compile(r"[0-9零〇○一二两兩三四五六七八九]")
NUMERIC_CHAR_RE = re.compile(r"[0-9零〇○一二两兩三四五六七八九十百千万萬亿億]")
NUMERIC_GROUP_SEPARATOR_RE = re.compile(r"[,，]")

LABELS = {
    "章": "Chương",
    "卷": "Quyển",
    "集": "Tập",
    "节": "Tiết",
    "節": "Tiết",
    "幕": "Màn",
    "回": "Hồi",
    "折": "Chiết",
}
COMMON_ANCHORS = frozenset("的了是不在")
CAPTURE_SOURCES = frozenset({"n", "y", "L", "ne", "pn", "vp", "hv", "w"})


class RuleParseError(Exception):
    """A rule line failed validation; carries the JS error message verbatim."""


class CaptureToken(TypedDict):
    type: Literal["capture"]
    sources: list[str]
    min: int
    max: int


class LiteralToken(TypedDict):
    type: Literal["literal"]
    value: str


class GroupToken(TypedDict):
    type: Literal["group"]
    alternatives: list[str]
    optional: bool


Token = Union[CaptureToken, LiteralToken, GroupToken]


class Anchor(TypedDict):
    value: str
    tokenIndex: int


class Rule(TypedDict):
    pattern: str
    translation: str
    line: int
    tokens: list[Token]
    anchor: Anchor
    specificity: int
    wildcardCount: int
    order: int


class RuleError(TypedDict):
    line: int
    reason: str
    source: str


class ParseRulesResult(TypedDict):
    rules: list[Rule]
    errors: list[RuleError]


class EngineDicts(TypedDict):
    pa: dict[str, str]
    vp: dict[str, str]
    names: dict[str, str]
    pronouns: dict[str, str]


class EngineOptions(TypedDict, total=False):
    separator: str
    vpMaxLen: int


class CaptureMatch(TypedDict):
    len: int
    raw: str
    value: str


class WalkHit(TypedDict):
    end: int
    captures: list[CaptureMatch]


class MatchHit(TypedDict):
    start: int
    end: int
    source: str
    translation: str
    pattern: str
    line: int


class WildcardItem(TypedDict):
    rule: Rule
    anchor: str
    min_offset: int
    max_offset: int


def first_meaning(value: object, separator: str) -> str:
    """Return the first sense of a dictionary value (``/`` or ``¦`` split)."""
    text = "" if value is None else str(value)
    end = len(text)
    for sep in (separator or "/", "¦"):
        if not sep:
            continue
        at = text.find(sep)
        if at >= 0 and at < end:
            end = at
    return text[:end].strip()


def digit_of(char: str) -> int | None:
    if re.fullmatch(r"[0-9]", char):
        return int(char)
    return NUM_DIGITS.get(char)


def year_to_latin(raw: str) -> str | None:
    out: list[str] = []
    for char in raw:
        digit = digit_of(char)
        if digit is None:
            return None
        out.append(str(digit))
    return "".join(out)


def chinese_to_latin(raw: str) -> str | None:
    """Convert a Chinese numeral / year string into Latin digits."""
    if re.fullmatch(r"[0-9]+", raw):
        return str(int(raw))
    if not re.search(r"[十百千万萬亿億]", raw):
        return year_to_latin(raw)

    total = 0
    section = 0
    number = 0
    saw = False
    for char in raw:
        digit = digit_of(char)
        if digit is not None:
            number = digit
            saw = True
            continue
        small = SMALL_UNITS.get(char)
        if small:
            section += (number or 1) * small
            number = 0
            saw = True
            continue
        large = LARGE_UNITS.get(char)
        if large:
            section += number
            total += (section or 1) * large
            section = 0
            number = 0
            saw = True
            continue
        return None
    return str(total + section + number) if saw else None


def _parse_range(raw: str | None, default_max: int) -> tuple[int, int]:
    if not raw:
        return 1, default_max
    match = re.fullmatch(r"(\d+)(?:-(\d+))?", raw)
    if not match:
        raise RuleParseError("range không hợp lệ")
    minimum = int(match.group(1))
    maximum = int(match.group(2) or match.group(1))
    if minimum < 1 or maximum < minimum or maximum > 64:
        raise RuleParseError("range không hợp lệ")
    return minimum, maximum


def parse_capture(raw: str) -> CaptureToken:
    split = raw.split(":")
    if len(split) > 2:
        raise RuleParseError("token không hợp lệ")
    sources = split[0].split("|")
    if not sources or any(source not in CAPTURE_SOURCES for source in sources):
        raise RuleParseError(f"token lạ: <{raw}>")
    fixed = len(sources) == 1 and sources[0] in ("L", "hv")
    minimum, maximum = _parse_range(split[1] if len(split) > 1 else "", 1 if fixed else 12)
    if fixed and (minimum != 1 or maximum != 1):
        raise RuleParseError(f"token <{sources[0]}> chỉ nhận 1 ký tự")
    return {
        "type": "capture",
        "sources": sources,
        "min": minimum,
        "max": maximum,
    }


def parse_group(pattern: str, start: int) -> tuple[GroupToken, int]:
    end = start + 1
    escaped = False
    while end < len(pattern):
        char = pattern[end]
        if escaped:
            escaped = False
            end += 1
            continue
        if char == "\\":
            escaped = True
            end += 1
            continue
        if char == ")":
            break
        end += 1
    if end >= len(pattern):
        raise RuleParseError("thiếu dấu )")
    body = pattern[start + 1 : end]
    alternatives = [re.sub(r"\\([|()])", r"\1", alt) for alt in body.split("|")]
    if any(not alt for alt in alternatives):
        raise RuleParseError("nhóm rỗng")
    optional = pattern[end + 1 : end + 2] == "?"
    token: GroupToken = {
        "type": "group",
        "alternatives": alternatives,
        "optional": optional,
    }
    return token, end + (2 if optional else 1)


def parse_pattern(pattern: str) -> list[Token]:
    tokens: list[Token] = []
    literal = ""

    def flush() -> None:
        nonlocal literal
        if literal:
            token: LiteralToken = {"type": "literal", "value": literal}
            tokens.append(token)
            literal = ""

    i = 0
    while i < len(pattern):
        char = pattern[i]
        if char == "<":
            flush()
            end = pattern.find(">", i + 1)
            if end < 0:
                raise RuleParseError("thiếu dấu >")
            tokens.append(parse_capture(pattern[i + 1 : end]))
            i = end + 1
        elif char == "(":
            flush()
            token, i = parse_group(pattern, i)
            tokens.append(token)
        else:
            literal += char
            i += 1
    flush()
    return tokens


def get_anchor(tokens: list[Token]) -> Anchor | None:
    candidates: list[Anchor] = []
    for i, token in enumerate(tokens):
        if token["type"] == "literal" and token["value"]:
            candidates.append({"value": token["value"], "tokenIndex": i})
        elif token["type"] == "group" and not token["optional"]:
            for value in token["alternatives"]:
                candidates.append({"value": value, "tokenIndex": i})

    def sort_key(candidate: Anchor) -> tuple[int, int]:
        common = 1 if len(candidate["value"]) == 1 and candidate["value"] in COMMON_ANCHORS else 0
        return common, -len(candidate["value"])

    candidates.sort(key=sort_key)
    return candidates[0] if candidates else None


def compile_rule(pattern: str, translation: str, line: int) -> Rule:
    tokens = parse_pattern(pattern)
    captures = [token for token in tokens if token["type"] == "capture"]
    anchor = get_anchor(tokens)
    if anchor is None:
        raise RuleParseError("rule không có neo")
    if not captures:
        raise RuleParseError("rule không có wildcard")

    used = {int(n) for n in re.findall(r"\{(\d+)\}", translation)}
    if any(n >= len(captures) for n in used):
        raise RuleParseError("placeholder không tồn tại")
    if any(i not in used for i in range(len(captures))):
        raise RuleParseError("capture không được dùng")

    specificity = 0
    for token in tokens:
        if token["type"] == "literal":
            specificity += len(token["value"])
        elif token["type"] == "group":
            specificity += max(len(alt) for alt in token["alternatives"])
        else:
            specificity += token["max"]

    return {
        "pattern": pattern,
        "translation": translation,
        "line": line,
        "tokens": tokens,
        "anchor": anchor,
        "specificity": specificity,
        "wildcardCount": len(captures),
        "order": 0,
    }


def parse_rules(text: str) -> ParseRulesResult:
    rules: list[Rule] = []
    errors: list[RuleError] = []
    seen: set[str] = set()
    for index, raw in enumerate(re.split(r"\r?\n", text or "")):
        line = raw.strip()
        if not line or line.startswith("#") or line.startswith("//"):
            continue
        eq = line.find("=")
        if eq < 1:
            errors.append({"line": index + 1, "reason": "thiếu dấu =", "source": raw})
            continue
        pattern = line[:eq].strip()
        translation = line[eq + 1 :].strip()
        if not translation:
            errors.append({"line": index + 1, "reason": "bản dịch rỗng", "source": raw})
            continue
        if pattern in seen:
            errors.append({"line": index + 1, "reason": "duplicate", "source": raw})
            continue
        try:
            rules.append(compile_rule(pattern, translation, index + 1))
            seen.add(pattern)
        except RuleParseError as exc:
            errors.append({"line": index + 1, "reason": str(exc), "source": raw})

    rules.sort(key=lambda rule: (-rule["specificity"], rule["wildcardCount"], rule["pattern"]))
    return {"rules": rules, "errors": errors}


def _normalize_dict(data: object) -> dict[str, str]:
    if isinstance(data, dict):
        return dict(data)
    return {}


def lookup_source(
    source: str, raw: str, dictionaries: EngineDicts, separator: str
) -> str | None:
    if source == "n":
        if not re.fullmatch(rf"[{NUMERIC_CHARS}]+", raw):
            return None
        return chinese_to_latin(raw)
    if source == "y":
        if not re.fullmatch(r"[0-9零〇○一二两兩三四五六七八九]+", raw):
            return None
        return year_to_latin(raw)
    if source == "L":
        return LABELS.get(raw) if len(raw) == 1 else None
    if source == "hv":
        if len(raw) != 1 or raw not in dictionaries["pa"]:
            return None
        return first_meaning(dictionaries["pa"][raw], separator)

    if source == "ne":
        order = [dictionaries["names"]]
    elif source == "pn":
        order = [dictionaries["pronouns"]]
    elif source == "vp":
        order = [dictionaries["vp"]]
    else:
        order = [dictionaries["names"], dictionaries["pronouns"], dictionaries["vp"]]
    for dictionary in order:
        if raw in dictionary:
            return first_meaning(dictionary[raw], separator)
    return None


def capture_matches(
    token: CaptureToken,
    text: str,
    position: int,
    dictionaries: EngineDicts,
    separator: str,
) -> list[CaptureMatch]:
    available = min(token["max"], len(text) - position)
    matches: list[CaptureMatch] = []
    for source in token["sources"]:
        for length in range(available, token["min"] - 1, -1):
            raw = text[position : position + length]
            value = lookup_source(source, raw, dictionaries, separator)
            if value is not None:
                matches.append({"len": length, "raw": raw, "value": value})
        if matches:
            break
    return matches


def match_rule(
    rule: Rule,
    text: str,
    start: int,
    dictionaries: EngineDicts,
    separator: str,
) -> MatchHit | None:
    tokens = rule["tokens"]

    def walk(
        token_index: int, position: int, captures: list[CaptureMatch]
    ) -> WalkHit | None:
        if token_index == len(tokens):
            return {"end": position, "captures": captures}
        token = tokens[token_index]
        if token["type"] == "literal":
            value = token["value"]
            if text.startswith(value, position):
                return walk(token_index + 1, position + len(value), captures)
            return None
        if token["type"] == "group":
            for value in token["alternatives"]:
                if not text.startswith(value, position):
                    continue
                hit = walk(token_index + 1, position + len(value), captures)
                if hit:
                    return hit
            return walk(token_index + 1, position, captures) if token["optional"] else None
        for candidate in capture_matches(token, text, position, dictionaries, separator):
            hit = walk(
                token_index + 1,
                position + candidate["len"],
                captures + [candidate],
            )
            if hit:
                return hit
        return None

    hit = walk(0, start, [])
    if not hit or hit["end"] <= start:
        return None

    end = hit["end"]
    continues_number_before = start > 0 and (
        bool(NUMERIC_CHAR_RE.search(text[start - 1]))
        or (
            start > 1
            and bool(NUMERIC_GROUP_SEPARATOR_RE.fullmatch(text[start - 1]))
            and bool(NUMERIC_CHAR_RE.search(text[start - 2]))
        )
    )
    continues_number_after = end < len(text) and (
        bool(NUMERIC_CHAR_RE.search(text[end]))
        or (
            end + 1 < len(text)
            and bool(NUMERIC_GROUP_SEPARATOR_RE.fullmatch(text[end]))
            and bool(NUMERIC_CHAR_RE.search(text[end + 1]))
        )
    )

    first = tokens[0]
    last = tokens[-1]
    if (
        first["type"] == "capture"
        and any(source in ("n", "y") for source in first["sources"])
        and continues_number_before
    ):
        return None
    if (
        last["type"] == "capture"
        and any(source in ("n", "y") for source in last["sources"])
        and continues_number_after
    ):
        return None

    translation = rule["translation"]
    for index, capture in enumerate(hit["captures"]):
        translation = translation.replace("{" + str(index) + "}", capture["value"])
    return {
        "start": start,
        "end": end,
        "source": text[start:end],
        "translation": translation,
        "pattern": rule["pattern"],
        "line": rule["line"],
    }


class RuleEngine:
    """Compiled rule set exposing the userscript's ``matchAt`` operation."""

    def __init__(
        self,
        rules: list[Rule],
        errors: list[RuleError],
        dictionaries: EngineDicts,
        separator: str,
        vp_max_len: int,
    ) -> None:
        self.rules = rules
        self.errors = errors
        self._dictionaries = dictionaries
        self._separator = separator
        self._vp_max_len = vp_max_len
        self._direct: dict[str, list[Rule]] = {}
        self._wildcard_first: list[Rule] = []
        self._wildcard_index: dict[str, list[WildcardItem]] = {}
        self._max_wildcard_prefix = 0
        self._build_index()

    def _build_index(self) -> None:
        for order, rule in enumerate(self.rules):
            rule["order"] = order
            first = rule["tokens"][0]
            if first["type"] == "literal" and first["value"]:
                self._direct.setdefault(first["value"][0], []).append(rule)
            elif first["type"] == "group" and not first["optional"]:
                for alternative in first["alternatives"]:
                    self._direct.setdefault(alternative[0], []).append(rule)
            else:
                self._wildcard_first.append(rule)
                min_offset = 0
                max_offset = 0
                anchors: list[str] | None = None
                for token in rule["tokens"]:
                    if token["type"] == "literal" and token["value"]:
                        anchors = [token["value"]]
                        break
                    if token["type"] == "group" and not token["optional"]:
                        anchors = token["alternatives"]
                        break
                    if token["type"] == "capture":
                        min_offset += token["min"]
                        max_offset += token["max"]
                    elif token["type"] == "group":
                        max_offset += max(len(alt) for alt in token["alternatives"])
                if anchors:
                    self._max_wildcard_prefix = max(self._max_wildcard_prefix, max_offset)
                    for anchor in anchors:
                        item: WildcardItem = {
                            "rule": rule,
                            "anchor": anchor,
                            "min_offset": min_offset,
                            "max_offset": max_offset,
                        }
                        self._wildcard_index.setdefault(anchor[0], []).append(item)

    def _longest_vp_at(self, text: str, start: int) -> int:
        maximum = min(len(text) - start, self._vp_max_len)
        for length in range(maximum, 0, -1):
            if text[start : start + length] in self._dictionaries["vp"]:
                return length
        return 0

    def match_at(self, text: str, start: int) -> MatchHit | None:
        if start >= len(text):
            return None
        candidates = list(self._direct.get(text[start], []))
        seen = {rule["order"] for rule in candidates}
        lookahead_end = min(len(text) - start - 1, self._max_wildcard_prefix)
        for offset in range(1, lookahead_end + 1):
            indexed = self._wildcard_index.get(text[start + offset])
            if not indexed:
                continue
            for item in indexed:
                if (
                    offset < item["min_offset"]
                    or offset > item["max_offset"]
                    or item["rule"]["order"] in seen
                ):
                    continue
                if not text.startswith(item["anchor"], start + offset):
                    continue
                seen.add(item["rule"]["order"])
                candidates.append(item["rule"])
        # Defensive fallback for a future wildcard-only syntax. Current
        # validation requires a literal anchor, so this normally stays empty.
        if not self._wildcard_index:
            candidates.extend(self._wildcard_first)
        candidates.sort(key=lambda rule: rule["order"])

        for rule in candidates:
            hit = match_rule(rule, text, start, self._dictionaries, self._separator)
            if not hit:
                continue
            if self._longest_vp_at(text, start) >= hit["end"] - start:
                continue
            return hit
        return None


def create_rule_engine(
    rule_text: str,
    input_dictionaries: Mapping[str, object] | None,
    options: EngineOptions | None = None,
) -> RuleEngine:
    """Compile ``rule.txt`` into an engine, mirroring ``VPRuleEngine.create``."""
    parsed = parse_rules(rule_text)
    dictionaries: EngineDicts = {
        "pa": _normalize_dict(input_dictionaries.get("pa") if input_dictionaries else None),
        "vp": _normalize_dict(input_dictionaries.get("vp") if input_dictionaries else None),
        "names": _normalize_dict(input_dictionaries.get("names") if input_dictionaries else None),
        "pronouns": _normalize_dict(
            input_dictionaries.get("pronouns") if input_dictionaries else None
        ),
    }
    opts = options or {}
    separator = opts.get("separator") or "/"
    vp_max_len = opts.get("vpMaxLen") or 64
    return RuleEngine(
        parsed["rules"],
        parsed["errors"],
        dictionaries,
        separator,
        vp_max_len,
    )


__all__ = [
    "LABELS",
    "MatchHit",
    "RuleEngine",
    "RuleParseError",
    "chinese_to_latin",
    "create_rule_engine",
    "parse_rules",
    "year_to_latin",
]
