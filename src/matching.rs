//! String normalization and similarity, used to decide whether a candidate
//! cover actually belongs to the track that is playing.
//!
//! This exists because remote catalogs do not reliably signal "no match" — the
//! iTunes Search API in particular answers a query for a record it does not
//! carry with a confident, completely unrelated album. Every candidate from a
//! search-based source is therefore checked against the MPRIS metadata, which
//! is the one thing we know to be true.

/// Lowercase, strip accents-ish, drop punctuation, collapse whitespace, and
/// remove the edition/remaster noise that stops otherwise-identical titles from
/// matching (`"Devil's Night (Deluxe Edition)"` vs `"Devil’s Night"`).
pub fn normalize(s: &str) -> String {
    const NOISE: [&str; 12] = [
        "deluxe edition",
        "deluxe version",
        "deluxe",
        "remastered",
        "remaster",
        "explicit version",
        "explicit",
        "special edition",
        "expanded edition",
        "anniversary edition",
        "bonus track version",
        "original motion picture soundtrack",
    ];

    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let ch = match ch {
            // Typographic characters that differ between metadata sources.
            '\u{2018}' | '\u{2019}' | '\u{02BC}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            c => c,
        };
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else {
            out.push(' ');
        }
    }

    let mut cleaned = out;
    for noise in NOISE {
        // NOISE entries are already in normalized form (lowercase, spaced).
        cleaned = cleaned.replace(noise, " ");
    }

    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Similarity in `0.0..=1.0`, combining token overlap with edit distance.
///
/// Token overlap alone treats "Greatest Hits" and "Hits Greatest" as identical;
/// edit distance alone punishes reordering too harshly. Taking the better of
/// the two is forgiving of both without accepting unrelated strings.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (normalize(a), normalize(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    token_overlap(&a, &b).max(edit_ratio(&a, &b))
}

/// Jaccard index over whitespace tokens.
fn token_overlap(a: &str, b: &str) -> f64 {
    let ta: Vec<&str> = a.split(' ').collect();
    let tb: Vec<&str> = b.split(' ').collect();

    let shared = ta.iter().filter(|t| tb.contains(t)).count();
    let union = ta.len() + tb.len() - shared;
    if union == 0 {
        return 0.0;
    }
    shared as f64 / union as f64
}

/// `1 - (levenshtein / max_len)`.
fn edit_ratio(a: &str, b: &str) -> f64 {
    let ca: Vec<char> = a.chars().collect();
    let cb: Vec<char> = b.chars().collect();
    let longest = ca.len().max(cb.len());
    if longest == 0 {
        return 1.0;
    }
    let dist = levenshtein(&ca, &cb);
    1.0 - (dist as f64 / longest as f64)
}

/// Two-row Levenshtein — we only ever compare short strings, but this keeps
/// allocation proportional to the shorter side rather than to their product.
fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];

    for (i, ac) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let cost = usize::from(ac != bc);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_typographic_and_edition_noise() {
        assert_eq!(normalize("Devil\u{2019}s Night"), "devil s night");
        assert_eq!(normalize("Devil's Night (Deluxe Edition)"), "devil s night");
    }

    #[test]
    fn matching_albums_score_high() {
        assert!(similarity("Devil\u{2019}s Night", "Devils Night") > 0.8);
        assert!(similarity("D12", "D12") > 0.99);
    }

    /// The exact failure this module exists to prevent: iTunes answering a
    /// query for "D12 - Devil's Night" with a Boccherini cello record.
    #[test]
    fn unrelated_albums_score_low() {
        let s = similarity(
            "Devil's Night",
            "Boccherini: Cello Concertos, Stabat Mater & Quintet",
        );
        assert!(s < 0.4, "expected a low score, got {s}");

        let s = similarity(
            "D12",
            "Ophelie Gaillard, Sandrine Piau & Pulcinella Orchestra",
        );
        assert!(s < 0.4, "expected a low score, got {s}");
    }

    #[test]
    fn reordered_tokens_still_match() {
        assert!(similarity("Greatest Hits", "Hits, Greatest") > 0.6);
    }
}
