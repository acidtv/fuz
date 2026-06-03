use std::borrow::Cow;

use memchr::{memchr, memchr2};

/// Per-needle-char bonus when that char lands at a word boundary. A boundary
/// is position 0, a position preceded by a non-alphanumeric byte (space, `_`,
/// `-`, `.`, …), or a CamelCase transition — a lowercase byte immediately
/// followed by an uppercase byte (e.g. `r`→`D` in `CountryDivision`).
const BOUNDARY_BONUS: i32 = 5;
/// Penalty per byte of "excess spread" beyond a contiguous substring match.
/// A literal substring has zero excess spread.
const SPREAD_PENALTY: i32 = 1;
/// Floor score for any subsequence match whose gaps between consecutive
/// matched positions are entirely alphanumeric — i.e. the haystack only
/// crosses a non-alphanumeric byte where the needle itself contains one.
/// Sized so a within-word subseq lifts clearly above cross-word matches in
/// the ranking, even without a boundary hit (e.g. `medtl` against
/// `immediately`: 5 needle chars spread over 8 bytes, no boundary hit → raw
/// formula gives -3, but the within-word floor lifts it to +61). Cross-word
/// matches (gaps contain non-alphanumeric bytes the needle did not dictate)
/// do not get this floor — they fall back to the raw boundary/spread formula
/// and can go negative, which ranks cross-word junk like `requires` formed
/// from `current ... acquires` below real matches.
const SINGLE_WORD_BASE: i32 = 64;
/// Floor score for a case-insensitive literal substring match. Sized so that
/// any literal match strictly beats any subsequence-only match: a within-word
/// subsequence's score is bounded above by SINGLE_WORD_BASE +
/// BOUNDARY_BONUS * needle_len, which for a 64-char needle is 384; we set
/// the floor well above that.
const LITERAL_BASE: i32 = 1024;

/// `pos` is at a word boundary in `hay`: position 0, preceded by a
/// non-alphanumeric byte, or a CamelCase transition (lowercase → uppercase).
/// Caller must ensure `pos < hay.len()`.
#[inline(always)]
fn boundary_at(hay: &[u8], pos: usize) -> bool {
    debug_assert!(pos < hay.len());
    if pos == 0 {
        return true;
    }
    let prev = unsafe { *hay.get_unchecked(pos - 1) };
    if !prev.is_ascii_alphanumeric() {
        return true;
    }
    let cur = unsafe { *hay.get_unchecked(pos) };
    prev.is_ascii_lowercase() && cur.is_ascii_uppercase()
}

#[inline(always)]
fn score_alignment(
    needle_len: usize,
    first_pos: usize,
    last_pos: usize,
    boundary_hits: i32,
    start_boundary: bool,
    within_one_word: bool,
) -> i32 {
    let spread = (last_pos - first_pos) as i32;
    let min_spread = (needle_len as i32) - 1;
    let excess = (spread - min_spread).max(0);
    if excess == 0 {
        LITERAL_BASE + if start_boundary { BOUNDARY_BONUS } else { 0 }
    } else if within_one_word {
        SINGLE_WORD_BASE + BOUNDARY_BONUS * boundary_hits - SPREAD_PENALTY * excess
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

    /// Returns Some((score, start)) if `hay` contains the needle as a
    /// (case-insensitive) subsequence. `start` is the 0-based byte offset in
    /// `hay` where the best-scoring alignment's first needle char lands;
    /// callers use it to emit a vimgrep-style column. Higher score = better
    /// match. Scoring is three-tier:
    /// - **Literal substring match** (highest tier): score = LITERAL_BASE + boundary_bonus
    ///   at the match position. Any literal match strictly beats any non-literal match.
    /// - **Within-word subsequence** (middle tier): every byte in the gaps
    ///   between consecutive matched positions is alphanumeric. Non-alphanumeric
    ///   bytes at matched positions are fine — those were dictated by the needle
    ///   (e.g. needle `CountryDiv(` against `CountryDivision(`: the `(` at the
    ///   final matched position is forced by the needle, not an unwanted cross-
    ///   word jump). Score = SINGLE_WORD_BASE + BOUNDARY_BONUS * boundary_hits
    ///   - SPREAD_PENALTY * excess. The floor keeps these clearly above
    ///   cross-word matches in the ranking.
    /// - **Cross-word subsequence** (lowest tier): some gap between matched
    ///   positions contains a non-alphanumeric byte. Score = BOUNDARY_BONUS *
    ///   boundary_hits - SPREAD_PENALTY * excess. Can go negative, ranking
    ///   junk like `requires` from `current ... acquires` below real matches.
    ///
    /// Implementation: **multi-start greedy.** Single-pass greedy from position 0
    /// picks the leftmost occurrence of needle[0], which often produces a bad
    /// alignment when the same byte appears earlier than the "real" cluster
    /// (e.g. needle 'rqrs' on `let n_str = ... requires` picks 'r' from `n_str`).
    /// Instead, iterate every needle[0] position and try greedy from each;
    /// keep the best score (and the start position that produced it).
    /// Early-exit when a literal lands at a word boundary (best possible).
    /// If greedy from one position fails (a later needle char is missing),
    /// all later starts also fail (their suffix is a subset), so we break.
    ///
    /// For empty needles, returns Some((0, 0)).
    #[inline]
    pub fn match_score(&self, hay: &[u8]) -> Option<(i32, usize)> {
        if self.lo.is_empty() {
            return Some((0, 0));
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
    fn match_score_ascii(&self, hay: &[u8]) -> Option<(i32, usize)> {
        let n = self.lo.len();
        let first_lo = self.lo[0];
        let first_hi = self.hi[0];
        let best_possible = LITERAL_BASE + BOUNDARY_BONUS;
        let mut best: Option<(i32, usize)> = None;
        let mut search_from = 0usize;

        loop {
            let slice = unsafe { hay.get_unchecked(search_from..) };
            let off = match memchr2(first_lo, first_hi, slice) {
                Some(o) => o,
                None => return best,
            };
            let start = search_from + off;
            let start_boundary = boundary_at(hay, start);

            // Greedy forward from `start`.
            let mut pos = start + 1;
            let mut last_pos = start;
            let mut boundary_hits: i32 = if start_boundary { 1 } else { 0 };
            let mut within_one_word = true;
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
                if within_one_word {
                    for j in pos..abs {
                        if !unsafe { *hay.get_unchecked(j) }.is_ascii_alphanumeric() {
                            within_one_word = false;
                            break;
                        }
                    }
                }
                last_pos = abs;
                if boundary_at(hay, abs) {
                    boundary_hits += 1;
                }
                pos = abs + 1;
            }
            if !completed {
                // Any later start has p'_{i-1} >= this run's p_{i-1}, so the missing
                // needle char is also unreachable. Safe to break.
                return best;
            }

            let score = score_alignment(
                n, start, last_pos, boundary_hits, start_boundary, within_one_word,
            );
            if score == best_possible {
                return Some((score, start));
            }
            best = Some(match best {
                None => (score, start),
                Some((b, bs)) => if score > b { (score, start) } else { (b, bs) },
            });
            search_from = start + 1;
        }
    }

    fn match_score_slow(&self, hay: &[u8]) -> Option<(i32, usize)> {
        let n = self.lo.len();
        let folded: Cow<[u8]> = if self.case_sensitive {
            Cow::Borrowed(hay)
        } else {
            Cow::Owned(hay.iter().map(|&b| b.to_ascii_lowercase()).collect())
        };
        let first = self.lo[0];
        let best_possible = LITERAL_BASE + BOUNDARY_BONUS;
        let mut best: Option<(i32, usize)> = None;
        let mut search_from = 0usize;

        loop {
            let off = match memchr(first, &folded[search_from..]) {
                Some(o) => o,
                None => return best,
            };
            let start = search_from + off;
            let start_boundary = boundary_at(hay, start);

            let mut pos = start + 1;
            let mut last_pos = start;
            let mut boundary_hits: i32 = if start_boundary { 1 } else { 0 };
            let mut within_one_word = true;
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
                if within_one_word {
                    for j in pos..abs {
                        if !hay[j].is_ascii_alphanumeric() {
                            within_one_word = false;
                            break;
                        }
                    }
                }
                last_pos = abs;
                if boundary_at(hay, abs) {
                    boundary_hits += 1;
                }
                pos = abs + 1;
            }
            if !completed {
                return best;
            }

            let score = score_alignment(
                n, start, last_pos, boundary_hits, start_boundary, within_one_word,
            );
            if score == best_possible {
                return Some((score, start));
            }
            best = Some(match best {
                None => (score, start),
                Some((b, bs)) => if score > b { (score, start) } else { (b, bs) },
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
        let tight = m.match_score(b"pattern").unwrap().0;
        let loose = m.match_score(b"pAAAAAAtAAAAAAtAAAAArAAAAAn").unwrap().0;
        assert!(tight > loose, "tight={} loose={}", tight, loose);
    }

    #[test]
    fn word_boundary_bonus() {
        let m = Matcher::new("foo");
        let underscore = m.match_score(b"my_foo").unwrap().0; // foo after _: word boundary
        let inline = m.match_score(b"barfoo").unwrap().0;      // foo buried: no boundary on f
        assert!(underscore > inline, "underscore={} inline={}", underscore, inline);
    }

    #[test]
    fn case_insensitive_same_score() {
        // Matching is case-insensitive, so uniformly lower- and uniformly
        // upper-case haystacks score identically. (A CamelCase haystack
        // would score higher — CamelCase transitions are real boundaries;
        // see camelcase_transition_counts_as_boundary.)
        let m = Matcher::new("pttrn");
        assert_eq!(m.match_score(b"pattern"), m.match_score(b"PATTERN"));
    }

    #[test]
    fn subseq_within_one_word_scores_positive() {
        // `rqrs` clustering inside `requires` is a real match. Score must be
        // clearly positive so it ranks above cross-word junk.
        let m = Matcher::new("rqrs");
        let s = m.match_score(b"            ... requires ...").unwrap().0;
        assert!(s > 0, "score was {s}");
    }

    #[test]
    fn within_word_subseq_without_boundary_hit_scores_positive() {
        // `medtl` against `immediately` lands the first needle char at byte 1
        // (no leading word boundary), so boundary_hits=0. The raw boundary/
        // spread formula gives a negative score, but the entire match span is
        // inside one alphanumeric run — the within-word floor must keep it
        // positive so it ranks above cross-word matches.
        let m = Matcher::new("medtl");
        let s = m.match_score(b"  immediately follows").unwrap().0;
        assert!(s > 0, "score was {s}");
    }

    #[test]
    fn within_word_no_boundary_scores_below_within_word_with_boundary() {
        // Both lines match within a single word, but `rqrs` in `requires`
        // hits the leading word boundary while `medtl` in `immediately`
        // doesn't. The boundary-hit alignment must rank higher.
        let with_boundary = Matcher::new("rqrs")
            .match_score(b"    requires foo")
            .unwrap()
            .0;
        let without_boundary = Matcher::new("medtl")
            .match_score(b"  immediately follows")
            .unwrap()
            .0;
        assert!(
            with_boundary > without_boundary,
            "with_boundary={with_boundary} without_boundary={without_boundary}"
        );
    }

    #[test]
    fn camelcase_transition_counts_as_boundary() {
        // A lowercase byte immediately followed by an uppercase byte starts
        // a new "word". Without this, `gun` aligning over `getUserName`
        // would hit only one boundary (the leading 'g'); with it, the U
        // after 't' and the N after 'r' each add a BOUNDARY_BONUS.
        let m = Matcher::new("gun");
        let camel = m.match_score(b"getUserName").unwrap().0;
        let flat = m.match_score(b"getusername").unwrap().0;
        assert!(camel > flat, "camel={camel} flat={flat}");
    }

    #[test]
    fn camelcase_lifts_cross_word_match() {
        // `clscntrdiv` matching `class CountryDivision`: c@start, C after
        // space, and D after y (CamelCase) — three boundary hits. Without
        // CamelCase support, the D would not count and the score would be
        // 5 lower. Either way the score must be clearly positive.
        let m = Matcher::new("clscntrdiv");
        let s = m.match_score(b"class CountryDivision:").unwrap().0;
        assert!(s > 5, "score was {s}");
    }

    #[test]
    fn needle_with_punctuation_inside_one_word_scores_positive() {
        // `CountryDiv(` matched against `class CountryDivision():` aligns
        // `CountryDiv` contiguously inside the identifier, then jumps to the
        // `(` after `…ision`. The `(` is a non-alphanumeric byte in the span,
        // but it's the matched position for needle's own `(` — not an unwanted
        // cross-word jump. Must score > 0 to rank above cross-word matches.
        let m = Matcher::new("CountryDiv(");
        let s = m.match_score(b"class CountryDivision():").unwrap().0;
        assert!(s > 0, "score was {s}");
    }

    #[test]
    fn subseq_spread_across_words_scores_nonpositive() {
        // `requires` matching via chars from `current` + `acquires` is junk
        // and must rank below within-word matches — scoring it non-positive
        // ensures it sits below the within-word floor.
        let m = Matcher::new("requires");
        let s = m.match_score(b"beat the current cutoff. Slow-path acquires").unwrap().0;
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
            .unwrap().0;
        let spread = m.match_score(b"        .require_git(false)").unwrap().0;
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
        let lit_score = m.match_score(b"extern int *errno_location").unwrap().0;
        // "err is not yet": e,r,r... n in "not"... o in "not". subseq matches, no literal.
        let subseq_score = m.match_score(b"err is not yet").unwrap().0;
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
        let at_boundary = m.match_score(b"my_foo_bar").unwrap().0;
        let buried = m.match_score(b"myfoobar").unwrap().0;
        assert!(at_boundary > buried);
        assert!(buried >= LITERAL_BASE); // still a literal match
    }

    #[test]
    fn empty_needle_scores_zero() {
        let m = Matcher::new("");
        assert_eq!(m.match_score(b"anything"), Some((0, 0)));
        assert_eq!(m.match_score(b""), Some((0, 0)));
    }

    #[test]
    fn returns_column_of_best_alignment() {
        // Literal "foo" appears at byte offset 3 in "my_foo_bar"; that's the
        // best alignment, so the returned start must be 3.
        let m = Matcher::new("foo");
        let (_, start) = m.match_score(b"my_foo_bar").unwrap();
        assert_eq!(start, 3);
    }

    #[test]
    fn multi_start_returns_winning_start() {
        // Multi-start picks the tight cluster inside "requires" (offset 28),
        // not the leftmost 'r' in "n_str" (offset 8). The start returned must
        // point at the alignment that actually won.
        let m = Matcher::new("rqrs");
        let (_, start) = m
            .match_score(b"    let n_str = args.next() requires a value")
            .unwrap();
        assert_eq!(start, 28);
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
