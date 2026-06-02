use std::borrow::Cow;

use memchr::{memchr, memchr2};

/// Per-needle-char bonus when that char lands at a word boundary
/// (preceded by a non-alphanumeric byte, or at position 0). Tuned so that a
/// tight subsequence cluster contained in one word (1 boundary hit at the
/// start, small excess spread) scores strictly positive.
const BOUNDARY_BONUS: i32 = 5;
/// Penalty per byte of "excess spread" beyond a contiguous substring match.
/// A literal substring has zero excess spread. Kept low so that a tight
/// subsequence cluster inside one word (e.g. needle 'rqrs' matching inside
/// 'requires', excess 3) stays positive — main.rs treats score > 0 as the
/// "real match" threshold and filters out everything below.
const SPREAD_PENALTY: i32 = 1;
/// Floor score for a case-insensitive literal substring match. Sized so that
/// any literal match strictly beats any subsequence-only match: a subsequence
/// match's score is bounded above by BOUNDARY_BONUS * needle_len, which for a
/// 64-char needle is 256; we set the floor well above that.
const LITERAL_BASE: i32 = 1024;

#[inline(always)]
fn score_alignment(
    needle_len: usize,
    first_pos: usize,
    last_pos: usize,
    boundary_hits: i32,
    start_boundary: bool,
) -> i32 {
    let spread = (last_pos - first_pos) as i32;
    let min_spread = (needle_len as i32) - 1;
    let excess = (spread - min_spread).max(0);
    if excess == 0 {
        LITERAL_BASE + if start_boundary { BOUNDARY_BONUS } else { 0 }
    } else {
        BOUNDARY_BONUS * boundary_hits - SPREAD_PENALTY * excess
    }
}

pub struct Matcher {
    pub lo: Vec<u8>,
    pub hi: Vec<u8>,
    pub unique_lo: Vec<u8>,
    pub unique_hi: Vec<u8>,
    pub ascii_only: bool,
    pub case_sensitive: bool,
}

impl Matcher {
    /// Smart case: a needle with any ASCII uppercase byte triggers a
    /// case-sensitive search; an all-lowercase (or no-cased) needle stays
    /// case-insensitive. In the case-sensitive branch, `lo` and `hi` hold the
    /// original bytes (identical), so the `memchr2(lo, hi, ...)` calls on the
    /// hot path degenerate to a single-byte search without any branching, and
    /// the slow-path haystack-folding step is skipped.
    pub fn new(needle: &str) -> Self {
        let bytes = needle.as_bytes();
        let ascii_only = bytes.is_ascii();
        let case_sensitive = bytes.iter().any(|b| b.is_ascii_uppercase());
        let mut lo = Vec::with_capacity(bytes.len());
        let mut hi = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if case_sensitive {
                lo.push(b);
                hi.push(b);
            } else {
                let l = b.to_ascii_lowercase();
                lo.push(l);
                hi.push(if l.is_ascii_alphabetic() { l ^ 0x20 } else { l });
            }
        }
        let mut unique_lo = Vec::new();
        let mut unique_hi = Vec::new();
        for i in 0..lo.len() {
            if !unique_lo.contains(&lo[i]) {
                unique_lo.push(lo[i]);
                unique_hi.push(hi[i]);
            }
        }
        Matcher { lo, hi, unique_lo, unique_hi, ascii_only, case_sensitive }
    }

    /// Returns Some(score) if `hay` contains the needle as a (case-insensitive) subsequence.
    /// Higher score = better match. Scoring is two-tier:
    /// - **Literal substring match** (highest tier): score = LITERAL_BASE + boundary_bonus
    ///   at the match position. Any literal match strictly beats any non-literal match.
    /// - **Subsequence-only match** (lower tier): score = BOUNDARY_BONUS * boundary_hits
    ///   - SPREAD_PENALTY * (spread - min_spread). Tighter and more boundary-aligned
    ///   alignments score higher.
    ///
    /// Implementation: **multi-start greedy.** Single-pass greedy from position 0
    /// picks the leftmost occurrence of needle[0], which often produces a bad
    /// alignment when the same byte appears earlier than the "real" cluster
    /// (e.g. needle 'rqrs' on `let n_str = ... requires` picks 'r' from `n_str`).
    /// Instead, iterate every needle[0] position and try greedy from each;
    /// keep the best score. Early-exit when a literal lands at a word boundary
    /// (best possible). If greedy from one position fails (a later needle char
    /// is missing), all later starts also fail (their suffix is a subset), so
    /// we break the iteration.
    ///
    /// For empty needles, returns Some(0).
    #[inline]
    pub fn match_score(&self, hay: &[u8]) -> Option<i32> {
        if self.lo.is_empty() {
            return Some(0);
        }
        if self.lo.len() > hay.len() {
            return None;
        }
        if self.ascii_only {
            self.match_score_ascii(hay)
        } else {
            self.match_score_slow(hay)
        }
    }

    #[inline(always)]
    fn match_score_ascii(&self, hay: &[u8]) -> Option<i32> {
        let n = self.lo.len();
        let first_lo = self.lo[0];
        let first_hi = self.hi[0];
        let best_possible = LITERAL_BASE + BOUNDARY_BONUS;
        let mut best: Option<i32> = None;
        let mut search_from = 0usize;

        loop {
            let slice = unsafe { hay.get_unchecked(search_from..) };
            let off = match memchr2(first_lo, first_hi, slice) {
                Some(o) => o,
                None => return best,
            };
            let start = search_from + off;
            let start_boundary = start == 0 || !hay[start - 1].is_ascii_alphanumeric();

            // Greedy forward from `start`.
            let mut pos = start + 1;
            let mut last_pos = start;
            let mut boundary_hits: i32 = if start_boundary { 1 } else { 0 };
            let mut completed = true;
            for i in 1..n {
                let inner = unsafe { hay.get_unchecked(pos..) };
                let off2 = match memchr2(self.lo[i], self.hi[i], inner) {
                    Some(o) => o,
                    None => {
                        completed = false;
                        break;
                    }
                };
                let abs = pos + off2;
                last_pos = abs;
                let prev = unsafe { *hay.get_unchecked(abs - 1) };
                if !prev.is_ascii_alphanumeric() {
                    boundary_hits += 1;
                }
                pos = abs + 1;
            }
            if !completed {
                // Any later start has p'_{i-1} >= this run's p_{i-1}, so the missing
                // needle char is also unreachable. Safe to break.
                return best;
            }

            let score = score_alignment(n, start, last_pos, boundary_hits, start_boundary);
            if score == best_possible {
                return Some(score);
            }
            best = Some(match best {
                None => score,
                Some(b) => b.max(score),
            });
            search_from = start + 1;
        }
    }

    fn match_score_slow(&self, hay: &[u8]) -> Option<i32> {
        let n = self.lo.len();
        let folded: Cow<[u8]> = if self.case_sensitive {
            Cow::Borrowed(hay)
        } else {
            Cow::Owned(hay.iter().map(|&b| b.to_ascii_lowercase()).collect())
        };
        let first = self.lo[0];
        let best_possible = LITERAL_BASE + BOUNDARY_BONUS;
        let mut best: Option<i32> = None;
        let mut search_from = 0usize;

        loop {
            let off = match memchr(first, &folded[search_from..]) {
                Some(o) => o,
                None => return best,
            };
            let start = search_from + off;
            let start_boundary = start == 0 || !hay[start - 1].is_ascii_alphanumeric();

            let mut pos = start + 1;
            let mut last_pos = start;
            let mut boundary_hits: i32 = if start_boundary { 1 } else { 0 };
            let mut completed = true;
            for i in 1..n {
                let off2 = match memchr(self.lo[i], &folded[pos..]) {
                    Some(o) => o,
                    None => {
                        completed = false;
                        break;
                    }
                };
                let abs = pos + off2;
                last_pos = abs;
                if !hay[abs - 1].is_ascii_alphanumeric() {
                    boundary_hits += 1;
                }
                pos = abs + 1;
            }
            if !completed {
                return best;
            }

            let score = score_alignment(n, start, last_pos, boundary_hits, start_boundary);
            if score == best_possible {
                return Some(score);
            }
            best = Some(match best {
                None => score,
                Some(b) => b.max(score),
            });
            search_from = start + 1;
        }
    }

    #[inline]
    pub fn prefilter_passes(&self, buf: &[u8]) -> bool {
        for i in 0..self.unique_lo.len() {
            if memchr2(self.unique_lo[i], self.unique_hi[i], buf).is_none() {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_basic() {
        let m = Matcher::new("pttrn");
        assert!(m.match_score(b"pattern").is_some());
        assert!(m.match_score(b"PaTTern").is_some());
        assert!(m.match_score(b"a pttrn b").is_some());
        assert!(m.match_score(b"p_t_t_r_n").is_some());
        assert!(!m.match_score(b"random").is_some());
        assert!(!m.match_score(b"ptrn").is_some());
        assert!(!m.match_score(b"parturition").is_some());
    }

    #[test]
    fn order_matters() {
        let m = Matcher::new("abc");
        assert!(m.match_score(b"a-b-c").is_some());
        assert!(!m.match_score(b"c-b-a").is_some());
    }

    #[test]
    fn empty_needle() {
        let m = Matcher::new("");
        assert!(m.match_score(b"").is_some());
        assert!(m.match_score(b"anything").is_some());
    }

    #[test]
    fn non_letters() {
        let m = Matcher::new("a.b");
        assert!(m.match_score(b"a.b").is_some());
        assert!(m.match_score(b"aXX.YYb").is_some());
        assert!(!m.match_score(b"a-b").is_some());
    }

    #[test]
    fn prefilter() {
        let m = Matcher::new("xyz");
        assert!(m.prefilter_passes(b"x y z all here"));
        assert!(!m.prefilter_passes(b"only x and y"));
    }

    #[test]
    fn match_score_returns_some_iff_subseq() {
        let m = Matcher::new("pttrn");
        for hay in [b"pattern".as_ref(), b"PaTTern", b"a pttrn b", b"p_t_t_r_n"] {
            assert!(m.match_score(hay).is_some(), "should match: {:?}", hay);
        }
        for hay in [b"random".as_ref(), b"ptrn", b"parturition"] {
            assert!(m.match_score(hay).is_none(), "should NOT match: {:?}", hay);
        }
    }

    #[test]
    fn tighter_alignment_scores_higher() {
        let m = Matcher::new("pttrn");
        // Both have a single boundary-hit (the leading 'p' at position 0); the
        // tighter alignment wins on spread alone. Filler chars are alphanumeric
        // so no extra boundary bonuses fire.
        let tight = m.match_score(b"pattern").unwrap();
        let loose = m.match_score(b"pAAAAAAtAAAAAAtAAAAArAAAAAn").unwrap();
        assert!(tight > loose, "tight={} loose={}", tight, loose);
    }

    #[test]
    fn word_boundary_bonus() {
        let m = Matcher::new("foo");
        let underscore = m.match_score(b"my_foo").unwrap(); // foo after _: word boundary
        let inline = m.match_score(b"barfoo").unwrap();      // foo buried: no boundary on f
        assert!(underscore > inline, "underscore={} inline={}", underscore, inline);
    }

    #[test]
    fn case_insensitive_same_score() {
        let m = Matcher::new("pttrn");
        assert_eq!(m.match_score(b"pattern"), m.match_score(b"PaTTern"));
    }

    #[test]
    fn subseq_within_one_word_scores_positive() {
        // `rqrs` clustering inside `requires` is a real match the user wants
        // to see. Score must be > 0 so the main.rs filter keeps it.
        let m = Matcher::new("rqrs");
        let s = m.match_score(b"            ... requires ...").unwrap();
        assert!(s > 0, "score was {s}");
    }

    #[test]
    fn subseq_spread_across_words_scores_nonpositive() {
        // `requires` matching via chars from `current` + `acquires` is junk
        // and must not survive the main.rs `score > 0` filter.
        let m = Matcher::new("requires");
        let s = m.match_score(b"beat the current cutoff. Slow-path acquires").unwrap();
        assert!(s <= 0, "score was {s}");
    }

    #[test]
    fn multi_start_finds_tight_cluster() {
        // Greedy from position 0 would pick 'r' in "n_str", giving a huge spread
        // jumping forward to "requires". Multi-start finds the tight cluster
        // inside "requires" itself. The line with a tight rqrs cluster must beat
        // a line whose only rqrs alignment is spread out.
        let m = Matcher::new("rqrs");
        let tight = m
            .match_score(b"    let n_str = args.next() requires a value")
            .unwrap();
        let spread = m.match_score(b"        .require_git(false)").unwrap();
        assert!(
            tight > spread,
            "tight_cluster={} spread_match={}",
            tight,
            spread
        );
    }

    #[test]
    fn literal_beats_subseq() {
        // The greedy-leftmost subseq match in "extern int *errno_location"
        // grabs the 'e' in "extern", producing a loose spread. But the literal
        // "errno" is also in the line, so the literal-path must fire and dominate.
        let m = Matcher::new("errno");
        let lit_score = m.match_score(b"extern int *errno_location").unwrap();
        // "err is not yet": e,r,r... n in "not"... o in "not". subseq matches, no literal.
        let subseq_score = m.match_score(b"err is not yet").unwrap();
        assert!(
            lit_score > subseq_score,
            "literal={} subseq={}",
            lit_score,
            subseq_score
        );
        assert!(lit_score >= LITERAL_BASE);
    }

    #[test]
    fn literal_at_word_boundary_beats_buried_literal() {
        let m = Matcher::new("foo");
        let at_boundary = m.match_score(b"my_foo_bar").unwrap();
        let buried = m.match_score(b"myfoobar").unwrap();
        assert!(at_boundary > buried);
        assert!(buried >= LITERAL_BASE); // still a literal match
    }

    #[test]
    fn empty_needle_scores_zero() {
        let m = Matcher::new("");
        assert_eq!(m.match_score(b"anything"), Some(0));
        assert_eq!(m.match_score(b""), Some(0));
    }

    #[test]
    fn smart_case_lowercase_needle_is_case_insensitive() {
        let m = Matcher::new("foo");
        assert!(!m.case_sensitive);
        assert!(m.match_score(b"foo").is_some());
        assert!(m.match_score(b"FOO").is_some());
        assert!(m.match_score(b"Foo").is_some());
        assert!(m.match_score(b"fOo").is_some());
    }

    #[test]
    fn smart_case_uppercase_needle_is_case_sensitive_literal() {
        let m = Matcher::new("Foo");
        assert!(m.case_sensitive);
        assert!(m.match_score(b"Foo").is_some());
        assert!(m.match_score(b"my_Foo_bar").is_some());
        assert!(m.match_score(b"foo").is_none());
        assert!(m.match_score(b"FOO").is_none());
        assert!(m.match_score(b"fOo").is_none());
    }

    #[test]
    fn smart_case_mixed_case_needle_is_case_sensitive() {
        let m = Matcher::new("fOo");
        assert!(m.case_sensitive);
        assert!(m.match_score(b"fOo").is_some());
        assert!(m.match_score(b"a fOo b").is_some());
        assert!(m.match_score(b"foo").is_none());
        assert!(m.match_score(b"Foo").is_none());
    }

    #[test]
    fn smart_case_case_sensitive_subsequence() {
        // Uppercase in needle -> case-sensitive. "FB" subseq matches "FooBar"
        // (F, then B) but not "foobar".
        let m = Matcher::new("FB");
        assert!(m.case_sensitive);
        assert!(m.match_score(b"FooBar").is_some());
        assert!(m.match_score(b"foobar").is_none());
        assert!(m.match_score(b"fooBar").is_none()); // missing capital F
        assert!(m.match_score(b"Foobar").is_none()); // missing capital B
    }

    #[test]
    fn smart_case_lowercase_subsequence_still_case_insensitive() {
        let m = Matcher::new("fb");
        assert!(!m.case_sensitive);
        assert!(m.match_score(b"FooBar").is_some());
        assert!(m.match_score(b"foobar").is_some());
        assert!(m.match_score(b"FOOBAR").is_some());
    }

    #[test]
    fn smart_case_prefilter_respects_case() {
        let m = Matcher::new("Xyz");
        assert!(m.case_sensitive);
        assert!(m.prefilter_passes(b"X y z all here"));
        // Lowercase x must NOT satisfy the prefilter when needle has uppercase X.
        assert!(!m.prefilter_passes(b"x y z all here"));
    }

    #[test]
    fn smart_case_prefilter_lowercase_needle_unchanged() {
        let m = Matcher::new("xyz");
        assert!(!m.case_sensitive);
        assert!(m.prefilter_passes(b"X Y Z here"));
        assert!(m.prefilter_passes(b"x y z here"));
        assert!(!m.prefilter_passes(b"only x and y"));
    }

    #[test]
    fn smart_case_non_ascii_needle_with_uppercase_is_case_sensitive() {
        // Non-ASCII needle (Café) hits the slow path; the ASCII 'C' makes it
        // case-sensitive, so haystack folding must be skipped.
        let m = Matcher::new("Café");
        assert!(m.case_sensitive);
        assert!(!m.ascii_only);
        assert!(m.match_score("a Café b".as_bytes()).is_some());
        assert!(m.match_score("a café b".as_bytes()).is_none());
    }

    #[test]
    fn smart_case_non_ascii_needle_all_lowercase_is_case_insensitive() {
        let m = Matcher::new("café");
        assert!(!m.case_sensitive);
        assert!(!m.ascii_only);
        // Folding still kicks in on the ASCII part of the haystack.
        assert!(m.match_score("a Café b".as_bytes()).is_some());
        assert!(m.match_score("a café b".as_bytes()).is_some());
    }
}
