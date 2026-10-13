//! Finds where an edit's `old_string` sits in a file. Tiers from strict to forgiving, modeled
//! on Claude Code, Codex `apply_patch` and Aider; each tier must find exactly one place, and if a
//! tier finds several the search stops (a looser tier would only be more ambiguous). Similarity
//! scoring is never used to apply an edit — only to point at the closest text in error messages.
//! All matching happens on LF-normalized, BOM-free text; callers restore the file's format.

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strategy {
    Exact,
    Unicode,
    TrailingWhitespace,
    Indentation,
    LineNumbers,
    Unescaped,
}

impl Strategy {
    pub fn describe(self) -> Option<&'static str> {
        match self {
            Strategy::Exact => None,
            Strategy::Unicode => Some("matched after normalizing quotes/dashes/spaces"),
            Strategy::TrailingWhitespace => Some("matched ignoring trailing whitespace"),
            Strategy::Indentation => Some("matched with different indentation; new text re-indented to fit"),
            Strategy::LineNumbers => Some("matched after removing line-number prefixes copied from read output"),
            Strategy::Unescaped => Some("matched after un-escaping \\n / \\\" sequences"),
        }
    }
}

#[derive(Debug)]
pub struct Found {
    /// Byte range in the content to replace.
    pub start: usize,
    pub end: usize,
    /// Text to put there (re-indented / cleaned when the strategy required it).
    pub replacement: String,
    pub strategy: Strategy,
}

#[derive(Debug)]
pub enum Miss {
    /// Not found. `closest` is the most similar region: (first line, last line), 1-based.
    NotFound { closest: Option<(usize, usize)> },
    /// Found in several places (1-based start lines).
    Ambiguous { lines: Vec<usize>, strategy: Strategy },
}

pub fn line_of(content: &str, byte: usize) -> usize {
    content[..byte].matches('\n').count() + 1
}

fn line_starts(s: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(s.match_indices('\n').map(|(i, _)| i + 1));
    v
}

/// All non-overlapping exact occurrences.
pub fn exact_all(content: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    content.match_indices(needle).map(|(i, _)| i).collect()
}

/// `fuzzy` = false limits matching to exact + Unicode normalization (used when the file
/// changed on disk since Claude last read it, so its picture of the file may be stale).
pub fn find(content: &str, old: &str, new: &str, fuzzy: bool) -> Result<Found, Miss> {
    // 1. Exact.
    let exact = exact_all(content, old);
    match exact.len() {
        1 => return Ok(Found { start: exact[0], end: exact[0] + old.len(), replacement: new.to_string(), strategy: Strategy::Exact }),
        0 => {}
        _ => return Err(ambiguous(content, &exact, Strategy::Exact)),
    }

    // 2. Unicode punctuation (curly quotes, dashes, odd spaces) — offsets map back 1:1 per char.
    let (norm, map) = normalize_unicode(content);
    let norm_old = normalize_unicode(old).0;
    if norm != content || norm_old != old {
        let hits = exact_all(&norm, &norm_old);
        match hits.len() {
            1 => {
                return Ok(Found { start: map[hits[0]], end: map[hits[0] + norm_old.len()], replacement: new.to_string(), strategy: Strategy::Unicode });
            }
            0 => {}
            _ => return Err(ambiguous(content, &hits.iter().map(|&h| map[h]).collect::<Vec<_>>(), Strategy::Unicode)),
        }
    }
    if !fuzzy {
        return Err(Miss::NotFound { closest: closest_region(content, old) });
    }

    // 3-4. Line windows: ignoring trailing whitespace, then a uniform indentation shift.
    for strategy in [Strategy::TrailingWhitespace, Strategy::Indentation] {
        match line_match(content, old, new, strategy) {
            Ok(found) => return Ok(found),
            Err(m @ Miss::Ambiguous { .. }) => return Err(m),
            Err(_) => {}
        }
    }

    // 5. Line-number prefixes copied from read output ("12: code", "    12\tcode").
    if let Some(stripped_old) = strip_line_numbers(old) {
        let stripped_new = strip_line_numbers(new).unwrap_or_else(|| new.to_string());
        return find(content, &stripped_old, &stripped_new, true).map(|mut f| {
            f.strategy = Strategy::LineNumbers;
            f
        });
    }

    // 6. Over-escaped text ("a\\nb" meant as two lines), only if the file doesn't contain those escapes.
    if let Some(old_u) = unescape(old, content) {
        let new_u = unescape(new, content).unwrap_or_else(|| new.to_string());
        return find(content, &old_u, &new_u, true).map(|mut f| {
            f.strategy = Strategy::Unescaped;
            f
        });
    }

    Err(Miss::NotFound { closest: closest_region(content, old) })
}

fn ambiguous(content: &str, offsets: &[usize], strategy: Strategy) -> Miss {
    Miss::Ambiguous { lines: offsets.iter().map(|&o| line_of(content, o)).collect(), strategy }
}

fn unicode_char(c: char) -> char {
    match c {
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

/// Normalized text plus, for every byte of it (and one past the end), the byte offset in `s`.
fn normalize_unicode(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    for (i, c) in s.char_indices() {
        let n = unicode_char(c);
        for _ in 0..n.len_utf8() {
            map.push(i);
        }
        out.push(n);
    }
    map.push(s.len());
    (out, map)
}

/// Line-window matching under a normalization; replaces whole lines.
fn line_match(content: &str, old: &str, new: &str, strategy: Strategy) -> Result<Found, Miss> {
    let old_lines: Vec<&str> = old.trim_matches('\n').split('\n').collect();
    if old_lines.iter().all(|l| l.trim().is_empty()) {
        return Err(Miss::NotFound { closest: None });
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let n = old_lines.len();
    if n > lines.len() {
        return Err(Miss::NotFound { closest: None });
    }
    let clean = |l: &str| normalize_unicode(l).0.trim_end().to_string();
    let wanted: Vec<String> = old_lines.iter().map(|l| clean(l)).collect();
    let have: Vec<String> = lines.iter().map(|l| clean(l)).collect();

    let mut hits = Vec::new();
    for i in 0..=lines.len() - n {
        let window = &have[i..i + n];
        let ok = match strategy {
            Strategy::TrailingWhitespace => window == wanted.as_slice(),
            _ => uniform_shift(&wanted, window).is_some(),
        };
        if ok {
            hits.push(i);
        }
    }
    match hits.len() {
        0 => Err(Miss::NotFound { closest: None }),
        1 => {
            let i = hits[0];
            let starts = line_starts(content);
            let start = starts[i];
            let end = start + lines[i..i + n].iter().map(|l| l.len()).sum::<usize>() + (n - 1);
            let mut replacement = new.trim_matches('\n').to_string();
            if strategy == Strategy::Indentation {
                let (remove, add) = uniform_shift(&wanted, &have[i..i + n]).unwrap();
                replacement = replacement
                    .split('\n')
                    .map(|l| if l.trim().is_empty() { String::new() } else { format!("{add}{}", l.strip_prefix(remove.as_str()).unwrap_or(l)) })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            Ok(Found { start, end, replacement, strategy })
        }
        _ => Err(Miss::Ambiguous { lines: hits.iter().map(|i| i + 1).collect(), strategy }),
    }
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// If every non-empty line of `window` equals the matching `old` line with one fixed prefix
/// removed and another added, returns (removed, added). Aider's "uniform offset" rule: a block
/// that was merely shifted left/right matches; one with inconsistent indentation does not.
fn uniform_shift(old: &[String], window: &[String]) -> Option<(String, String)> {
    let k = old.iter().position(|l| !l.trim().is_empty())?;
    let (oi, wi) = (indent_of(&old[k]), indent_of(&window[k]));
    let (remove, add) = if let Some(extra) = wi.strip_prefix(oi) {
        (String::new(), extra.to_string())
    } else if let Some(extra) = oi.strip_prefix(wi) {
        (extra.to_string(), String::new())
    } else {
        (oi.to_string(), wi.to_string())
    };
    for (o, w) in old.iter().zip(window) {
        if o.trim().is_empty() && w.trim().is_empty() {
            continue;
        }
        let rest = o.strip_prefix(remove.as_str())?;
        if *w != format!("{add}{rest}") {
            return None;
        }
    }
    Some((remove, add))
}

static LINE_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\d+(?:: ?|\t|→|\| )").unwrap());

/// Removes "N: " / "N\t" prefixes when every non-empty line has one.
fn strip_line_numbers(s: &str) -> Option<String> {
    let lines: Vec<&str> = s.split('\n').collect();
    let numbered = lines.iter().filter(|l| !l.trim().is_empty());
    if numbered.clone().count() == 0 || !numbered.clone().all(|l| LINE_NUMBER.is_match(l)) {
        return None;
    }
    Some(lines.iter().map(|l| LINE_NUMBER.replace(l, "").into_owned()).collect::<Vec<_>>().join("\n"))
}

/// Turns literal escape sequences into the characters they stand for, but only when the file
/// itself doesn't use those escape sequences (otherwise they are real text).
fn unescape(s: &str, content: &str) -> Option<String> {
    let escapes = ["\\n", "\\\"", "\\t", "\\'"];
    if !escapes.iter().any(|e| s.contains(e) && !content.contains(e)) {
        return None;
    }
    let out = s.replace("\\r\\n", "\n").replace("\\n", "\n").replace("\\t", "\t").replace("\\\"", "\"").replace("\\'", "'");
    (out != s).then_some(out)
}

/// Character similarity in [0, 1] (whitespace-insensitive Levenshtein); for error hints only.
pub fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    let b: Vec<char> = b.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.len().max(b.len()) > 3000 {
        return if a == b { 1.0 } else { 0.0 };
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    1.0 - prev[b.len()] as f64 / a.len().max(b.len()) as f64
}

/// The region most like `old` (same number of lines, most lines equal after trimming, ties
/// broken by text similarity), shown in errors so the retry can copy the real text.
pub fn closest_region(content: &str, old: &str) -> Option<(usize, usize)> {
    let old = strip_line_numbers(old).unwrap_or_else(|| old.to_string());
    let old_lines: Vec<String> = old.trim_matches('\n').split('\n').map(|l| normalize_unicode(l.trim()).0).collect();
    let lines: Vec<String> = content.split('\n').map(|l| normalize_unicode(l.trim()).0).collect();
    let n = old_lines.len().min(lines.len());
    if n == 0 || old_lines.iter().all(|l| l.is_empty()) {
        return None;
    }
    let mut best: Option<(usize, usize)> = None; // (score, start)
    for i in 0..=lines.len() - n {
        let score = (0..n).filter(|&k| !old_lines[k].is_empty() && lines[i + k] == old_lines[k]).count();
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, i));
        }
    }
    if let Some((_, i)) = best {
        return Some((i + 1, i + n));
    }
    // No identical line: use the line most similar to the first meaningful line of old.
    let probe = old_lines.iter().find(|l| l.len() > 3)?;
    let (i, sim) = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.is_empty() && l.len() < 500)
        .map(|(i, l)| (i, similarity(probe, l)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    (sim >= 0.6).then_some((i + 1, (i + n).min(lines.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(content: &str, old: &str, new: &str) -> (String, Strategy) {
        let f = find(content, old, new, true).expect("should match");
        (format!("{}{}{}", &content[..f.start], f.replacement, &content[f.end..]), f.strategy)
    }

    #[test]
    fn exact() {
        let (out, s) = apply("a\nb\nc\n", "b", "B");
        assert_eq!(out, "a\nB\nc\n");
        assert_eq!(s, Strategy::Exact);
    }

    #[test]
    fn ambiguous_stops_search() {
        assert!(matches!(find("x\nx\n", "x", "y", true), Err(Miss::Ambiguous { .. })));
        // Unique only after ignoring trailing spaces, but the stricter tier already saw two.
        assert!(matches!(find("ab\nab \n", "ab", "c", true), Err(Miss::Ambiguous { .. })));
    }

    #[test]
    fn unicode_punctuation() {
        let file = "msg = “don’t panic” — ok\n";
        let (out, s) = apply(file, "msg = \"don't panic\" - ok", "msg = \"relax\"");
        assert_eq!(out, "msg = \"relax\"\n");
        assert_eq!(s, Strategy::Unicode);
    }

    #[test]
    fn trailing_whitespace() {
        let (out, s) = apply("fn a() {   \n  1\n}\n", "fn a() {\n  1\n}", "fn a() {\n  2\n}");
        assert_eq!(out, "fn a() {\n  2\n}\n");
        assert_eq!(s, Strategy::TrailingWhitespace);
    }

    #[test]
    fn uniform_indentation_shift_reindents_new_text() {
        let file = "class A {\n    run() {\n        go();\n    }\n}\n";
        let (out, s) = apply(file, "run() {\n    go();\n}", "run() {\n    stop();\n    go();\n}");
        assert_eq!(out, "class A {\n    run() {\n        stop();\n        go();\n    }\n}\n");
        assert_eq!(s, Strategy::Indentation);
    }

    #[test]
    fn inconsistent_indentation_is_not_matched() {
        let file = "if x:\n    a()\n        b()\n";
        assert!(matches!(find(file, "if x:\n    a()\n    b()", "", true), Err(Miss::NotFound { .. })));
    }

    #[test]
    fn line_number_prefixes() {
        let file = "one\ntwo\nthree\n";
        let (out, s) = apply(file, "2: two\n3: three", "2: TWO\n3: three");
        assert_eq!(out, "one\nTWO\nthree\n");
        assert_eq!(s, Strategy::LineNumbers);
    }

    #[test]
    fn over_escaped() {
        let (out, s) = apply("print(\"hi\")\nreturn 1\n", "print(\\\"hi\\\")\\nreturn 1", "print(\\\"bye\\\")\\nreturn 2");
        assert_eq!(out, "print(\"bye\")\nreturn 2\n");
        assert_eq!(s, Strategy::Unescaped);
    }

    #[test]
    fn real_escapes_in_file_are_not_unescaped() {
        let file = "s = \"a\\nb\"\n";
        let (out, s) = apply(file, "s = \"a\\nb\"", "s = \"c\\nd\"");
        assert_eq!(out, "s = \"c\\nd\"\n");
        assert_eq!(s, Strategy::Exact);
    }

    #[test]
    fn stale_file_only_allows_strict_tiers() {
        assert!(find("fn a() {   \n}\n", "fn a() {\n}", "", false).is_err());
    }

    #[test]
    fn not_found_points_at_closest_region() {
        let file = "a\nfn load() {\n  read();\n}\nz\n";
        match find(file, "fn load() {\n  fetch();\n}", "", true) {
            Err(Miss::NotFound { closest: Some((2, 4)) }) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}
