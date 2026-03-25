#!/usr/bin/env python3
"""Download SymSpell frequency dictionaries for spell-checking."""

import urllib.request
from pathlib import Path

DICTIONARIES = {
    "frequency_dictionary_en_82_765.txt": (
        "https://raw.githubusercontent.com/wolfgarbe/SymSpell/refs/heads/master/"
        "SymSpell.FrequencyDictionary/en-82_765.txt"
    ),
    "frequency_bigramdictionary_en_243_342.txt": (
        "https://raw.githubusercontent.com/wolfgarbe/SymSpell/refs/heads/master/"
        "SymSpell.FrequencyDictionary/en_bigrams.txt"
    ),
}

def main():
    out_dir = Path(__file__).resolve().parent.parent / "models" / "dictionaries"
    out_dir.mkdir(parents=True, exist_ok=True)

    print("Downloading symspell dictionaries...")
    for filename, url in DICTIONARIES.items():
        dest = out_dir / filename
        print(f"  {url} -> {dest}")
        urllib.request.urlretrieve(url, dest)

    print("Done.")
    for f in out_dir.iterdir():
        print(f"  {f.name}  ({f.stat().st_size:,} bytes)")

if __name__ == "__main__":
    main()
