//! Internal text utilities: whitespace normalisation, grapheme-aware
//! truncation, word / sentence segmentation, syllable counting, slugs and
//! Turkish-aware case folding.

use unicode_segmentation::UnicodeSegmentation;

/// Collapses every run of whitespace into one space and trims.
pub(crate) fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Number of extended grapheme clusters (what a reader perceives as characters).
pub(crate) fn grapheme_len(s: &str) -> usize {
    s.graphemes(true).count()
}

/// Truncates to at most `max` graphemes (including the trailing `…`), cutting
/// at the last word boundary when one exists in the second half of the text.
pub(crate) fn truncate_graphemes(s: &str, max: usize) -> String {
    let s = s.trim();
    if grapheme_len(s) <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1; // room for the ellipsis
    let mut end = 0;
    for (count, (idx, g)) in s.grapheme_indices(true).enumerate() {
        if count == budget {
            break;
        }
        end = idx + g.len();
    }
    let head = &s[..end];
    // Prefer a word boundary if it doesn't throw away more than half the text.
    let cut = head
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .filter(|&i| grapheme_len(&head[..i]) * 2 >= budget)
        .map_or(head, |i| &head[..i]);
    let cut = cut.trim_end_matches(|c: char| c.is_whitespace() || ",;:-–—|·".contains(c));
    format!("{cut}…")
}

/// Unicode words (letters/digits; punctuation dropped).
pub(crate) fn words(s: &str) -> impl Iterator<Item = &str> {
    s.unicode_words()
}

/// Number of Unicode words.
pub(crate) fn word_count(s: &str) -> usize {
    s.unicode_words().count()
}

/// Sentences (trimmed, non-empty).
pub(crate) fn sentences(s: &str) -> Vec<&str> {
    s.unicode_sentences()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.chars().any(char::is_alphanumeric))
        .collect()
}

/// Turkish syllable count: one syllable per vowel (exact for Turkish).
pub(crate) fn syllables_tr(word: &str) -> usize {
    word.chars()
        .filter(|c| {
            matches!(
                c,
                'a' | 'e'
                    | 'ı'
                    | 'i'
                    | 'o'
                    | 'ö'
                    | 'u'
                    | 'ü'
                    | 'â'
                    | 'î'
                    | 'û'
                    | 'A'
                    | 'E'
                    | 'I'
                    | 'İ'
                    | 'O'
                    | 'Ö'
                    | 'U'
                    | 'Ü'
            )
        })
        .count()
        .max(1)
}

/// English syllable estimate: vowel groups, minus a silent final `e`.
pub(crate) fn syllables_en(word: &str) -> usize {
    let w: Vec<char> = word
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if w.is_empty() {
        return 1;
    }
    if w.len() <= 3 {
        return 1;
    }
    let is_vowel = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
    let mut count = 0;
    let mut prev = false;
    for &c in &w {
        let v = is_vowel(c);
        if v && !prev {
            count += 1;
        }
        prev = v;
    }
    let n = w.len();
    if w[n - 1] == 'e' && !(n >= 3 && w[n - 2] == 'l' && !is_vowel(w[n - 3])) && count > 1 {
        count -= 1;
    }
    count.max(1)
}

/// Lower-cases with Turkish rules and strips Turkish / circumflex diacritics so
/// `"İSTANBUL"`, `"istanbul"` and `"Istanbul"` compare equal (`ı`/`i` both → `i`).
pub(crate) fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'İ' | 'I' | 'ı' | 'î' | 'Î' => out.push('i'),
            'ç' | 'Ç' => out.push('c'),
            'ğ' | 'Ğ' => out.push('g'),
            'ö' | 'Ö' => out.push('o'),
            'ş' | 'Ş' => out.push('s'),
            'ü' | 'Ü' | 'û' | 'Û' => out.push('u'),
            'â' | 'Â' => out.push('a'),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// URL slug: folded, ASCII alphanumerics separated by single dashes.
pub(crate) fn slugify(s: &str) -> String {
    let folded = fold(s);
    let mut out = String::with_capacity(folded.len());
    let mut dash = false;
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    out
}

/// Whether `needle` occurs in `haystack` as a whole word sequence (after folding).
pub(crate) fn contains_phrase(haystack: &str, needle: &str) -> bool {
    count_phrase(haystack, needle) > 0
}

/// Word without a Turkish apostrophe suffix (`hosting'in` → `hosting`).
fn base_word(w: &str) -> &str {
    w.split(['\'', '’']).next().unwrap_or(w)
}

/// Number of (folded, whole-word) occurrences of `needle` in `haystack`.
pub(crate) fn count_phrase(haystack: &str, needle: &str) -> usize {
    let needle_words: Vec<String> = words(&fold(needle))
        .map(|w| base_word(w).to_owned())
        .collect();
    if needle_words.is_empty() {
        return 0;
    }
    let folded = fold(haystack);
    let hay: Vec<&str> = words(&folded).map(base_word).collect();
    hay.windows(needle_words.len())
        .filter(|w| w.iter().zip(&needle_words).all(|(a, b)| a == b))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_grapheme_safe() {
        // Family emoji is one grapheme made of several code points.
        let s = "👨‍👩‍👧‍👦👨‍👩‍👧‍👦👨‍👩‍👧‍👦";
        let t = truncate_graphemes(s, 2);
        assert_eq!(grapheme_len(&t), 2);
        assert!(t.ends_with('…'));
        assert_eq!(truncate_graphemes("kısa", 10), "kısa");
        let long = "Türkiye'nin en hızlı SSD hosting paketleri ve ücretsiz SSL sertifikası";
        let t = truncate_graphemes(long, 30);
        assert!(grapheme_len(&t) <= 30);
        assert!(t.ends_with('…'));
        assert!(!t.contains(" …"));
    }

    #[test]
    fn folding_and_slugs() {
        assert_eq!(fold("İSTANBUL Işık"), "istanbul isik");
        assert_eq!(
            slugify("Çok Hızlı  WordPress Hosting!"),
            "cok-hizli-wordpress-hosting"
        );
        assert_eq!(slugify("--"), "");
        assert!(contains_phrase(
            "En iyi WordPress Hosting burada",
            "wordpress hosting"
        ));
        assert!(!contains_phrase("hostingci", "hosting"));
        assert_eq!(count_phrase("vps vps VPS sunucu", "vps"), 3);
        assert!(contains_phrase("Acme Hosting'in paketleri", "Acme Hosting"));
    }

    #[test]
    fn syllables() {
        assert_eq!(syllables_tr("bilgisayar"), 4);
        assert_eq!(syllables_en("hosting"), 2);
        assert_eq!(syllables_en("the"), 1);
        assert_eq!(syllables_en("simple"), 2);
    }

    #[test]
    fn segmentation() {
        assert_eq!(word_count("Merhaba dünya, 3 GB disk."), 5);
        assert_eq!(sentences("Bir. İki? Üç!").len(), 3);
        assert_eq!(normalize_ws("  a \n\t b  "), "a b");
    }
}
