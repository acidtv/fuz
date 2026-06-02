use std::io::{BufWriter, Write};
use std::sync::Arc;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

mod matcher;
mod printer;
mod stats;
mod topk;
mod walker;

use matcher::Matcher;
use printer::emit_match;
use stats::Stats;
use topk::TopK;

static STATS: Stats = Stats::new();

const DEFAULT_TOP_N: usize = 20;

fn main() {
    install_sigpipe_default();

    let (needle, top_n) = match parse_args() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("usage: fuz [-n N] PATTERN");
            std::process::exit(2);
        }
    };

    let matcher = Matcher::new(&needle);
    let topk = Arc::new(TopK::new(top_n));
    let profile = std::env::var_os("FUZ_PROFILE").is_some();

    let t0 = std::time::Instant::now();
    walker::run(matcher, Arc::clone(&topk), &STATS);
    let wall = t0.elapsed();

    // Final dump: drain heap, sort score-desc + path-asc + line-asc, write through
    // a 64 KiB BufWriter. The explicit sort overrides the heap's internal tiebreaks
    // (which use path-desc to drive eviction) so users see alphabetical order within
    // tied scores.
    //
    // Quality filter: drop matches with score <= 0. The matcher is tuned so that
    // any subsequence cluster contained within a single word scores positive,
    // while spread-across-words subsequence "matches" (the common source of junk
    // results — e.g. `requires` matching via `current ... acquires`) go negative.
    // This filters the junk without hiding real subseq matches (e.g. `rqrs` finding
    // the cluster inside the word `requires`).
    let topk = Arc::try_unwrap(topk).ok().expect("topk has one ref after walker.run returns");
    let mut results = topk.into_sorted_best_first();
    results.retain(|r| r.score > 0);
    results.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.prefix.cmp(&b.prefix))
            .then_with(|| a.line_no.cmp(&b.line_no))
    });
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(64 * 1024, stdout.lock());
    // emit_match takes a Vec<u8> buffer; we reuse a small staging Vec to avoid
    // per-match heap allocs and write each formatted match through BufWriter.
    let mut staging: Vec<u8> = Vec::with_capacity(512);
    for r in results {
        staging.clear();
        emit_match(&mut staging, &r.prefix, r.line_no, r.col, &r.line);
        let _ = out.write_all(&staging);
    }
    let _ = out.flush();

    if profile {
        STATS.print(wall);
    }
}

fn parse_args() -> Result<(String, usize), String> {
    let mut args = std::env::args().skip(1);
    let mut needle: Option<String> = None;
    let mut top_n: usize = DEFAULT_TOP_N;
    while let Some(a) = args.next() {
        if a == "-n" {
            let n_str = args.next().ok_or_else(|| "-n requires a value".to_string())?;
            let n: usize = n_str
                .parse()
                .map_err(|_| format!("-n: invalid number: {n_str}"))?;
            if n == 0 {
                return Err("-n must be >= 1".to_string());
            }
            top_n = n;
        } else if a == "--" {
            needle = args.next();
            break;
        } else if a.starts_with('-') && a.len() > 1 {
            return Err(format!("unknown flag: {a}"));
        } else if needle.is_none() {
            needle = Some(a);
        } else {
            return Err("too many positional arguments".to_string());
        }
    }
    let needle = needle.ok_or_else(|| "missing PATTERN".to_string())?;
    if args.next().is_some() {
        return Err("too many positional arguments".to_string());
    }
    Ok((needle, top_n))
}

#[cfg(unix)]
fn install_sigpipe_default() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn install_sigpipe_default() {}
