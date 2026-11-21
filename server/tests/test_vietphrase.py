"""Contract tests for the Python port of vietphrase.user.js.

The fixtures are tiny inline dictionaries, so the rule engine, segmentation,
casing and token-map alignment all execute without network access.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from server.vietphrase import (
    DictionarySet,
    Settings,
    VietPhraseTranslator,
    chinese_to_latin,
    create_rule_engine,
    load_default_dictionaries,
    parse_dictionary,
    parse_rules,
    year_to_latin,
)
from server.vietphrase.translator import fix_vietnamese, load_dictionary_set


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("3", "3"),
        ("零三", "03"),
        ("二零二四", "2024"),
        ("十", "10"),
        ("二十三", "23"),
        ("一百零五", "105"),
        ("一万", "10000"),
        ("两亿", "200000000"),
        ("abc", None),
    ],
)
def test_chinese_to_latin(raw: str, expected: str | None) -> None:
    assert chinese_to_latin(raw) == expected


@pytest.mark.parametrize(
    ("raw", "expected"),
    [
        ("一九四九", "1949"),
        ("二〇", "20"),
        ("二零二a", None),
    ],
)
def test_year_to_latin(raw: str, expected: str | None) -> None:
    assert year_to_latin(raw) == expected


def test_parse_rules_sorts_by_specificity_and_reports_errors() -> None:
    parsed = parse_rules(
        "\n".join(
            [
                "<n:1-4>倍=gấp {0} lần",
                "A<n:1-4>倍=gấp {0} lần",
                "# comment",
                "không có dấu bằng",
                "x=",
                "<n:1-4>倍=duplicate {0}",
            ]
        )
    )

    assert [rule["pattern"] for rule in parsed["rules"]] == [
        "A<n:1-4>倍",
        "<n:1-4>倍",
    ]
    assert [(error["line"], error["reason"]) for error in parsed["errors"]] == [
        (4, "thiếu dấu ="),
        (5, "bản dịch rỗng"),
        (6, "duplicate"),
    ]


def test_rule_engine_number_capture_and_boundary_guard() -> None:
    engine = create_rule_engine("<n:1-3>倍=gấp {0} lần", {})

    hit = engine.match_at("3倍", 0)
    assert hit is not None
    assert hit["source"] == "3倍"
    assert hit["translation"] == "gấp 3 lần"
    assert hit["end"] == 2

    # A number capture must not start in the middle of a longer number.
    assert engine.match_at("13倍", 1) is None


def test_rule_engine_dictionary_capture_sources() -> None:
    engine = create_rule_engine(
        "\n".join(["<ne:1-3>说={0} nói", "<hv:1>国={0} Quốc"]),
        {"names": {"张三": "Trương Tam"}, "pa": {"中": "Trung"}},
    )

    name_hit = engine.match_at("张三说", 0)
    han_hit = engine.match_at("中国", 0)
    assert name_hit is not None and name_hit["translation"] == "Trương Tam nói"
    assert han_hit is not None and han_hit["translation"] == "Trung Quốc"


def test_parse_dictionary_filters_and_normalizes() -> None:
    text = "\n".join(
        [
            "# comment",
            "// comment",
            "=bad",
            "你好=Xin chào",
            "你=Bạn Co\u0301",
            "中国=Trung Quốc",
            "中=abc",
        ]
    )

    assert parse_dictionary(text) == {
        "你好": "Xin chào",
        "你": "Bạn Có",
        "中国": "Trung Quốc",
        "中": "abc",
    }
    assert parse_dictionary(text, "PA") == {"你": "Bạn Có", "中": "abc"}
    assert fix_vietnamese("Co\u0301") == "Có"


def test_load_dictionary_set_reads_all_five_files(tmp_path: Path) -> None:
    (tmp_path / "ChinesePhienAmWords.txt").write_text("你=Bạn", encoding="utf-8")
    (tmp_path / "Vietphrase.txt").write_text("你好=Xin chào", encoding="utf-8")
    (tmp_path / "Names.txt").write_text("张三=Trương Tam", encoding="utf-8")
    (tmp_path / "Pronouns.txt").write_text("我=tôi", encoding="utf-8")
    (tmp_path / "rule.txt").write_text("<n:1-3>倍=gấp {0} lần", encoding="utf-8")

    dictionaries = load_dictionary_set(tmp_path)

    assert dictionaries.pa == {"你": "Bạn"}
    assert dictionaries.vp == {"你好": "Xin chào"}
    assert dictionaries.names == {"张三": "Trương Tam"}
    assert dictionaries.pronouns == {"我": "tôi"}
    assert "倍" in dictionaries.rules


def test_translate_prefers_longest_vp_then_pa() -> None:
    translator = VietPhraseTranslator(
        DictionarySet(
            vp={"中国": "Trung Quốc", "中": "Trung"},
            pa={"国": "quốc", "好": "tốt"},
        )
    )

    assert translator.translate("中国") == "Trung Quốc"
    assert translator.translate("中国好") == "Trung Quốc tốt"


def test_translate_drops_dich_lieu_and_maps_punctuation() -> None:
    translator = VietPhraseTranslator(DictionarySet(pa={"他": "anh", "来": "đến"}))

    assert translator.translate("他来了。") == "Anh đến."


def test_translate_names_and_rules() -> None:
    name_translator = VietPhraseTranslator(
        DictionarySet(names={"张三": "Trương Tam"}, pa={"来": "đến"})
    )
    rule_translator = VietPhraseTranslator(DictionarySet(rules="<n:1-3>倍=gấp {0} lần"))

    assert name_translator.translate("张三来了") == "Trương Tam đến"

    result = rule_translator.translate_with_map("3倍")
    assert result.result == "Gấp 3 lần"
    assert [(token.han, token.viet) for token in result.token_map] == [
        ("3倍", "Gấp 3 lần")
    ]


def test_translate_casing_and_quote_placeholders() -> None:
    casing = VietPhraseTranslator(
        DictionarySet(vp={"你好": "Xin chào", "世界": "Thế giới"})
    )
    quoted = VietPhraseTranslator(DictionarySet(pa={"你": "Bạn", "好": "tốt"}))

    assert casing.translate("你好世界") == "Xin chào thế giới"
    assert quoted.translate("「你好」") == '"Bạn tốt"'


def test_settings_ngoac_and_motnghia() -> None:
    translator = VietPhraseTranslator(
        DictionarySet(vp={"你好": "Xin chào / Chào bạn"}),
        Settings(ngoac=True, motnghia=False),
    )

    assert translator.translate("你好") == "[Xin chào / Chào bạn]"


def test_bundled_dictionaries_load_offline() -> None:
    dictionaries = load_default_dictionaries()

    assert len(dictionaries.pa) > 10000
    assert len(dictionaries.vp) > 100000
    assert "你们中国" in dictionaries.vp
    assert "张三" in dictionaries.names
    assert "倍" in dictionaries.rules
    assert (
        VietPhraseTranslator(dictionaries).translate("夜深了，雨还在下。")
        == "Đêm đã khuya, mưa vẫn còn rơi."
    )


def test_no_han_text_is_returned_unchanged() -> None:
    translator = VietPhraseTranslator(DictionarySet(pa={"你": "Bạn"}))

    result = translator.translate_with_map("hello world")
    assert result.result == "hello world"
    assert result.token_map == []
