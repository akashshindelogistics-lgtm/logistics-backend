//! Text handling for the assistant's FULLTEXT retrieval (see
//! `docs/rag-integration.md`, option A): identifier normalisation on both
//! the index and the query side, and light query expansion (plurals and a
//! small logistics synonym list). Pure functions, no database.
//!
//! The problems this solves were measured on a real database: a partial
//! registration (`9387`), a spaced one (`MH 12 DM 9387`), a plural
//! (`cements`) and a synonym (`delayed`) all matched nothing.

/// Words that mean the same thing in this domain. Searching for any one of a
/// group also searches the rest. Lowercase.
const SYNONYMS: &[&[&str]] = &[
    &["late", "delayed", "delay", "overdue", "behind"],
    &["truck", "vehicle", "lorry"],
    &["stock", "inventory"],
    &["customer", "client"],
    &["low", "below", "reorder", "threshold"],
    &["expiring", "expiry", "expires", "expire", "expired"],
];

/// Split into runs of letters and runs of digits; anything else separates.
/// `MH12DM9387` becomes `MH`, `12`, `DM`, `9387`.
fn segments(token: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut last_digit: Option<bool> = None;
    for c in token.chars() {
        if !c.is_ascii_alphanumeric() {
            last_digit = None;
            out.push(String::new());
            continue;
        }
        let is_digit = c.is_ascii_digit();
        if last_digit != Some(is_digit) || out.is_empty() {
            out.push(String::new());
        }
        out.last_mut().unwrap().push(c);
        last_digit = Some(is_digit);
    }
    out.retain(|s| !s.is_empty());
    out
}

fn alnum_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
}

/// Whether `s` has the shape of an Indian registration number: two letters,
/// one or two digits, up to three letters, four digits (`MH12DM9387`,
/// `KA01A1234`, `MH121234`).
fn is_registration(s: &str) -> bool {
    let seg = segments(s);
    let kinds: Vec<bool> = seg
        .iter()
        .map(|p| p.chars().all(|c| c.is_ascii_digit()))
        .collect();
    let shape_ok = match seg.len() {
        // MH121234: letters, then 1-2 digits and the 4-digit serial run together.
        2 => kinds == [false, true] && (5..=6).contains(&seg[1].len()),
        4 => {
            kinds == [false, true, false, true]
                && seg[1].len() <= 2
                && seg[2].len() <= 3
                && seg[3].len() == 4
        }
        _ => false,
    };
    shape_ok && seg[0].len() == 2 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Identifier variants a person might type for the identifiers in `text`,
/// space-separated, for the `AiChunks.identifiers` column. For
/// `MH12DM9387` that is `MH12DM9387`, `MH 12 DM 9387`, `DM9387` and `9387`.
/// Only tokens containing a digit (at least 4 characters long) count.
pub fn identifier_variants(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    for token in alnum_tokens(text) {
        if token.len() < 4 || !token.chars().any(|c| c.is_ascii_digit()) {
            continue;
        }
        push(token.to_string());
        let seg = segments(token);
        if seg.len() < 2 || seg.len() > 5 {
            continue;
        }
        push(seg.join(" "));
        let last = seg.len() - 1;
        if seg[last].chars().all(|c| c.is_ascii_digit()) {
            if seg[last].len() >= 3 {
                push(seg[last].clone());
            }
            if !seg[last - 1].chars().all(|c| c.is_ascii_digit()) {
                push(format!("{}{}", seg[last - 1], seg[last]));
            }
        }
    }
    out.join(" ")
}

/// Join the pieces of a spaced registration (`MH 12 DM 9387`, `mh-12-dm-9387`)
/// back into one token, leaving every other word alone.
fn join_registrations(query: &str) -> Vec<String> {
    let words: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == '-')
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let mut joined = None;
        for n in (2..=4).rev() {
            if i + n <= words.len() {
                let candidate = words[i..i + n].concat();
                if is_registration(&candidate) {
                    joined = Some((candidate, n));
                    break;
                }
            }
        }
        match joined {
            Some((candidate, n)) => {
                out.push(candidate);
                i += n;
            }
            None => {
                out.push(words[i].to_string());
                i += 1;
            }
        }
    }
    out
}

/// Plural and singular forms of a lowercase word (without the word itself).
fn number_variants(w: &str) -> Vec<String> {
    if w.len() < 4 || !w.chars().all(|c| c.is_ascii_alphabetic()) {
        return Vec::new();
    }
    let mut v = Vec::new();
    if let Some(stem) = w.strip_suffix("ies") {
        v.push(format!("{stem}y"));
    } else if w.ends_with("ses") || w.ends_with("xes") || w.ends_with("ches") || w.ends_with("shes")
    {
        v.push(w[..w.len() - 2].to_string());
    } else if w.ends_with('s') && !w.ends_with("ss") {
        v.push(w[..w.len() - 1].to_string());
    } else if let Some(stem) = w.strip_suffix('y') {
        v.push(format!("{stem}ies"));
    } else {
        v.push(format!("{w}s"));
    }
    v
}

/// The string to give `MATCH(text) AGAINST (... IN NATURAL LANGUAGE MODE)`:
/// the question with spaced registrations joined, plus plural/singular forms
/// and synonyms of each word. Natural-language mode ORs the words and ranks
/// by relevance, so extra words widen recall without dropping exact hits.
pub fn expand_query(query: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && !terms.contains(&s) {
            terms.push(s);
        }
    };
    for word in join_registrations(query) {
        for token in alnum_tokens(&word) {
            let lower = token.to_ascii_lowercase();
            let mut forms = number_variants(&lower);
            forms.push(lower.clone());
            for v in &forms {
                push(v.clone());
            }
            for group in SYNONYMS {
                if forms.iter().any(|f| group.contains(&f.as_str())) {
                    for s in *group {
                        push((*s).to_string());
                    }
                }
            }
            push(token.to_string());
        }
    }
    terms.join(" ")
}

/// Terms to look up in the `identifiers` column: the digit-bearing words of
/// the question (after joining spaced registrations) that are at least 3
/// characters, as a boolean-mode query of quoted phrases (`"9387" "KA01"`).
/// Empty when the question has no identifier-like word.
pub fn identifier_query(query: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    for word in join_registrations(query) {
        for token in alnum_tokens(&word) {
            if token.len() >= 3 && token.chars().any(|c| c.is_ascii_digit()) {
                let t = format!("\"{token}\"");
                if !terms.contains(&t) {
                    terms.push(t);
                }
            }
        }
    }
    terms.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_cover_partial_and_spaced_registrations() {
        let v = identifier_variants("Vehicle MH12DM9387 is a truck");
        for want in ["MH12DM9387", "MH 12 DM 9387", "DM9387", "9387"] {
            assert!(
                v.split(' ').collect::<Vec<_>>().join(" ").contains(want),
                "{v}"
            );
        }
    }

    #[test]
    fn variants_ignore_plain_words_and_short_numbers() {
        assert_eq!(identifier_variants("Cement for 10 units in Pune"), "");
    }

    #[test]
    fn spaced_registration_is_joined() {
        assert_eq!(
            join_registrations("where is MH 12 DM 9387 now"),
            vec!["where", "is", "MH12DM9387", "now"]
        );
        assert_eq!(join_registrations("mh-12-dm-9387"), vec!["mh12dm9387"]);
        assert_eq!(join_registrations("KA 01 1234"), vec!["KA011234"]);
        assert_eq!(
            join_registrations("10 units of cement"),
            vec!["10", "units", "of", "cement"]
        );
    }

    #[test]
    fn expansion_adds_plurals_and_synonyms() {
        let q = expand_query("cements");
        assert!(q.split(' ').any(|t| t == "cement"), "{q}");
        let q = expand_query("delayed trucks");
        for want in ["late", "overdue", "truck", "vehicle", "lorry"] {
            assert!(q.split(' ').any(|t| t == want), "{want} missing in {q}");
        }
    }

    #[test]
    fn identifier_query_quotes_digit_words_only() {
        assert_eq!(identifier_query("truck 9387"), "\"9387\"");
        assert_eq!(identifier_query("MH 12 DM 9387"), "\"MH12DM9387\"");
        assert_eq!(identifier_query("late trucks"), "");
        assert_eq!(identifier_query("5 trucks"), "");
    }
}
