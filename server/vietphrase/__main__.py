"""Small CLI demo: ``python -m server.vietphrase ...``.

Examples::

    python -m server.vietphrase --text "你好世界"          # bundled dictionaries
    python -m server.vietphrase --dict-dir ./dicts --text "你好世界"
    python -m server.vietphrase --fetch --text "你好世界"   # refresh from upstream
    echo 你好世界 | python -m server.vietphrase
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .translator import (
    DEFAULT_DICT_DIR,
    DictionarySet,
    VietPhraseTranslator,
    fetch_default_dictionaries,
    load_default_dictionaries,
    load_dictionary_set,
)

_DEFAULT_CACHE = Path(__file__).resolve().parent / ".cache"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Vietphrase dictionary translator")
    parser.add_argument("--text", help="Chinese text to translate (reads stdin if omitted)")
    parser.add_argument("--dict-dir", help="directory holding the five userscript TXT files")
    parser.add_argument("--fetch", action="store_true", help="download dictionaries from upstream instead of using the bundled copies")
    parser.add_argument("--json", action="store_true", help="print the full token map as JSON")
    args = parser.parse_args(argv)

    dictionaries: DictionarySet
    if args.dict_dir:
        dictionaries = load_dictionary_set(args.dict_dir)
    elif args.fetch:
        dictionaries = fetch_default_dictionaries(
            cache_dir=_DEFAULT_CACHE, save_dir=DEFAULT_DICT_DIR
        )
    else:
        dictionaries = load_default_dictionaries()

    text = args.text if args.text is not None else sys.stdin.read()
    result = VietPhraseTranslator(dictionaries).translate_with_map(text)

    if args.json:
        import json

        print(
            json.dumps(
                {
                    "result": result.result,
                    "token_map": [
                        {"han": t.han, "viet": t.viet, "start": t.start, "end": t.end}
                        for t in result.token_map
                    ],
                },
                ensure_ascii=False,
                indent=2,
            )
        )
    else:
        print(result.result)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
