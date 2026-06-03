# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- `make build` — release build (`cargo build --release`). The release profile uses `lto = "fat"`, single codegen unit, `panic = "abort"`, and strips symbols; debug builds are much slower and will mislead any perf work.
- `make test` — runs `cargo test --release`. Tests assert on score *orderings* tuned for the release-mode matcher; prefer running them in release.
- Run a single test: `cargo test --release <test_name>` (e.g. `cargo test --release multi_start_finds_tight_cluster`).
- Run the binary against the current directory: `./target/release/fuz [-n N] PATTERN` (default `-n 20`).
- Profiling: `FUZ_PROFILE=1 ./target/release/fuz PATTERN` prints a per-phase CPU breakdown (I/O, binary detect, prefilter, search) to stderr after results. Use this rather than guessing where time goes.

## Architecture

`fuz` is a parallel, top-K fuzzy line search over a directory tree. Data flows through three layers; understanding the contract between them is essential before changing anything.

**Walker → Matcher → TopK pipeline** (`src/walker.rs`, `src/matcher.rs`, `src/topk.rs`):

1. **Walker** uses `ignore::WalkBuilder` (respects `.gitignore`, `.ignore`, hidden files) with `require_git(false)` so it works outside repos. Each worker thread reuses two `thread_local!` buffers (`READ_BUF`, `PREFIX_BUF`) to avoid per-file allocation. Per file: open → size check (`MAX_FILE_SIZE = 256 MiB`) → `posix_fadvise(SEQUENTIAL)` on Linux → read-to-end → NUL-byte binary probe on the first 8 KiB → optional whole-buffer prefilter for files > 64 KiB → line-by-line scan via `memchr_iter(b'\n', …)`. Skip counters and per-phase nanosecond timers feed the global `Stats`.

2. **Matcher** is a three-tier scorer (`score_alignment` in `src/matcher.rs`). **Literal substring matches** sit in a high band (`LITERAL_BASE = 1024` + boundary bonus) — any literal strictly beats any subsequence-only match. **Within-word subsequences** (all gap bytes between matched positions are alphanumeric) score `SINGLE_WORD_BASE + bonus_sum - SPREAD_PENALTY * excess`; the floor keeps them clearly above cross-word matches. **Cross-word subsequences** (some gap contains a non-alphanumeric byte) score `bonus_sum - SPREAD_PENALTY * excess` and can go negative, ranking junk like `requires` from `current … acquires` below real matches. A boundary is line start, any position preceded by a non-alphanumeric byte, *or* a CamelCase transition (lowercase→uppercase). `bonus_sum` uses **fzf-style consecutive-run inheritance**: each matched char's effective bonus is `max(own_boundary_bonus, prev_eff_bonus_if_adjacent)`. So a boundary spreads forward through the contiguous needle run that follows it — without this, `clscoundiv(` would rank `CountryDetailView(` above `CountryDivision(` because the needle's `v` happens to land on `View`'s `V` (a CamelCase boundary). Nothing is filtered out by score — TopK alone bounds visible results. The constants are interdependent; don't tweak `BOUNDARY_BONUS` / `SPREAD_PENALTY` / `LITERAL_BASE` / `SINGLE_WORD_BASE` without rerunning the score-ordering tests (`subseq_within_one_word_scores_positive`, `subseq_spread_across_words_scores_nonpositive`, `camelcase_transition_counts_as_boundary`, `contiguous_cluster_beats_scattered_boundary_matches`, `within_word_no_boundary_scores_below_within_word_with_boundary`).

   **If you change the ranking algorithm** (tiers, boundary rules, scoring constants), update the **Ranking** section of `README.md` to match. Keep that section simple and sweet — it's user-facing, not a spec.

   The search is **multi-start greedy**, not single-pass: iterating every `needle[0]` position and keeping the best score is what produces good alignments when the first needle char appears earlier than the "real" cluster (see `multi_start_finds_tight_cluster`). Early exits: any literal at a word boundary hits the maximum possible score; if greedy fails from start `i`, all later starts also fail (suffixes are subsets). ASCII-only needles take a fast path using `memchr2(lo, hi)` for case-insensitive matching without folding; otherwise a slow path folds the haystack once. `prefilter_passes` checks each unique needle byte is present anywhere in the buffer — a cheap pre-check before line-by-line scoring.

3. **TopK** is a single `Mutex<BinaryHeap<Reverse<Scored>>>` plus a lock-free `AtomicI32` cutoff. The hot path is `if score <= cutoff { return; }` — most candidates never touch the mutex. The heap stores `Reverse<Scored>` so the root is the *worst* kept entry (cheap eviction); when full, the cutoff is updated to the new worst score. `Ord for Scored` uses `(score, prefix, line_no)` with `prefix` ascending, but the tiebreak inside the heap is effectively path-*descending* (because of `Reverse`). The final sort in `main.rs` re-applies the user-visible order: score desc, then path asc, then line asc.

**Output** (`src/printer.rs`, `src/main.rs`): results are written through a 64 KiB `BufWriter`. `emit_match` formats `path:line_no:content\n` into a reusable staging `Vec<u8>` using a hand-rolled `write_u64` to avoid `format!`. The `prefix` field on each `Scored` is the pre-baked `b"path:"` bytes built once per file in `search_buffer`, so output formatting per match is just three `extend_from_slice` calls and one integer write.

**Allocator and signal handling** (`src/main.rs`): `MiMalloc` is the global allocator (measurable win under the high small-allocation rate of the read/match path). `SIGPIPE` is restored to the default handler so piping into `head` exits cleanly instead of panicking on a broken pipe.
