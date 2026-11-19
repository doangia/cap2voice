# vietphrase

Python port of the translation logic in
[`duongden/script-vietphrase-translator`](https://github.com/duongden/script-vietphrase-translator)
(`vietphrase.user.js`, v2.3.1). Dictionary-based Chinese → Vietnamese
translation in the Vietphrase "QuickTranslate" style, with no browser/DOM
dependency and no third-party Python packages (stdlib only).

## Layout

| File | Role |
| --- | --- |
| `rule_engine.py` | Port of `VPRuleEngine`: `rule.txt` parsing, number conversion, capture sources, specificity ordering, number-boundary guards, `match_at` |
| `translator.py` | Port of the translation pipeline: dictionary parsing, punctuation mapping, name/rule segmentation, longest-VP matching, casing/spacing, token map |
| `__init__.py` | Public API exports |
| `__main__.py` | CLI demo (`python -m server.vietphrase ...`) |
| `data/*.txt` | Dictionaries (downloaded separately, not committed — see below): PA, VP, Names, Pronouns, rule.txt (~30 MB) |

The browser-only parts of the userscript (MutationObserver, DOM rewriting,
clipboard, dictionary-manager UI, Settings storage) are intentionally omitted.

## Dictionaries (not bundled with the repo)

The five TXT files in `data/` come from the upstream userscript repo
[`duongden/script-vietphrase-translator`](https://github.com/duongden/script-vietphrase-translator)
and are **GPL-3.0 licensed data**. They are deliberately **not committed** to
this repo (see `.gitignore`): this repo is MIT, and the dictionaries stay the
user's own local copy.

First-time setup (downloads the five files into `data/`, ~30 MB, once):

```powershell
python -m server.vietphrase --fetch --text "夜深了，雨还在下。"
```

After that, plain runs work offline from `data/`. You can also copy the five
TXT files (`ChinesePhienAmWords.txt`, `Vietphrase.txt`, `Names.txt`,
`Pronouns.txt`, `rule.txt`) from the upstream repo into `data/` by hand.
If `data/` is missing, `load_default_dictionaries()` raises a `RuntimeError`
telling you exactly how to fetch them.

## Usage

After the one-time setup in the "Dictionaries" section above, translation
works offline from `data/`.

```python
from server.vietphrase import VietPhraseTranslator, load_default_dictionaries

translator = VietPhraseTranslator(load_default_dictionaries())

print(translator.translate("夜深了，雨还在下。"))
# Đêm đã khuya, mưa vẫn còn rơi.

result = translator.translate_with_map("第三章")
# result.result    -> "Đệ tam chương"
# result.token_map -> [TokenMapEntry(han=..., viet=..., start=..., end=...)]
```

`load_dictionary_set(path)` reads a different set of five TXT files;
`fetch_default_dictionaries(cache_dir=...)` downloads fresh copies from the
upstream repo and caches them locally. Network failures raise `RuntimeError`
with the failing URL (never silent).

CLI:

```powershell
python -m server.vietphrase --text "你好世界"                  # bundled data
python -m server.vietphrase --text "你好世界" --json
python -m server.vietphrase --dict-dir ./dicts --text "你好世界"
python -m server.vietphrase --fetch --text "你好世界"           # refresh from upstream
```

## Fidelity check

The port was compared against the original userscript run in Node.js with the
real dictionaries (`PA=16782 VP=627603 Names=186765 Pronouns=1452 Rules=587`)
on 37 Chinese sample sentences: translation output and token map matched
**37/37** in both cases.
