"""Vietphrase dictionary translator (Python port of ``vietphrase.user.js``).

Pure, DOM-free logic: a QuickTranslate-compatible rule engine plus a
dictionary-based Chinese→Vietnamese translator. Used by the local server as an
offline alternative to the network translation endpoint.
"""

from .rule_engine import (
    LABELS,
    RuleEngine,
    RuleParseError,
    chinese_to_latin,
    create_rule_engine,
    parse_rules,
    year_to_latin,
)
from .translator import (
    DEFAULT_DICT_DIR,
    DictionarySet,
    Settings,
    TokenMapEntry,
    TranslationResult,
    VietPhraseTranslator,
    fetch_default_dictionaries,
    load_default_dictionaries,
    load_dictionary_set,
    parse_dictionary,
)

__all__ = [
    "DEFAULT_DICT_DIR",
    "LABELS",
    "DictionarySet",
    "RuleEngine",
    "RuleParseError",
    "Settings",
    "TokenMapEntry",
    "TranslationResult",
    "VietPhraseTranslator",
    "chinese_to_latin",
    "create_rule_engine",
    "fetch_default_dictionaries",
    "load_default_dictionaries",
    "load_dictionary_set",
    "parse_dictionary",
    "parse_rules",
    "year_to_latin",
]
