# Symspell Word Segmentation Case Sensitivity Bug

**Date:** 2024-12-01
**Status:** Diagnosed, fix identified

## Summary

Symspell's `word_segmentation()` function is case-sensitive, causing catastrophic failure when input contains capital letters.

## Symptoms

- OCR output like `Sexualityofwomenwithexianervosa` (no spaces, concatenated words)
- After segmentation: `S ex u al it y of women with ex ian er v os a` (character soup)
- Some words correctly segmented (`of`, `women`, `with`) while others broken into single chars

## Root Cause

The symspell crate's `word_segmentation()` performs dictionary lookups that are **case-sensitive**.

When processing `Sexuality`:
1. Looks up "Sexuality" in dictionary → NOT FOUND (dictionary has lowercase "sexuality")
2. Falls back to character-by-character segmentation
3. Result: `S ex u al it y`

When processing `sexuality` (lowercase):
1. Looks up "sexuality" in dictionary → FOUND
2. Correctly identifies word boundary
3. Result: `sexuality` (preserved)

## Evidence

```
# With original case:
SEGMENT: 'Sexualityofwomenwithexianervosa' -> 'S ex u al it y of women with ex ian er v os a'

# With lowercase:
SEGMENT: 'Sexualityofwomenwithexianervosa' -> 'sexuality of women with ex ian er v os a'
```

Dictionary confirmed to contain the word:
```bash
$ grep -i "^sexuality " data/frequency_dictionary_en_82_765.txt
sexuality 5434339
```

## Fix

Lowercase the input before calling `word_segmentation()`:

```rust
fn segment_words(&self, text: &str) -> String {
    let Some(dict) = &self.dictionary else {
        return String::from(text);
    };

    text.split_whitespace()
        .map(|token| {
            if Self::needs_segmentation(token) {
                let lower = token.to_lowercase();
                dict.word_segmentation(&lower, 2).segmented_string
            } else {
                String::from(token)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
```

## Remaining Issues

1. **Lost capitalization**: The fix produces lowercase output. May want to restore original casing for proper nouns/sentence starts.

2. **OCR errors propagate**: If OCR misreads "anorexia" as "exia", segmentation can't fix it. The word "exia" isn't in the dictionary, so it still breaks into `ex ian er v os a`.

## Related Files

- `pdf-mash/src/ocr/postprocessor.rs` - `segment_words()` function
- `pdf-mash/src/ocr/config.rs` - `enable_segmentation` flag
- `pdf-mash/data/frequency_dictionary_en_82_765.txt` - symspell dictionary

## References

- [symspell crate](https://crates.io/crates/symspell)
- Original symspell algorithm: https://github.com/wolfgarbe/SymSpell
