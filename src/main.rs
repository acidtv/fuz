use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, Ordering};
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
use walker::{Limits, DEFAULT_MAX_FILE_SIZE, DEFAULT_MAX_LINE_LEN};

static STATS: Stats = Stats::new();

// Global cancellation flag. Flipped by SIGINT/SIGTERM handlers and by the
// stdout-hangup watchdog when the consumer closes the pipe. Checked once per
// file in the walker callback (cheap relaxed load) — granular enough since
// files are searched in microseconds-to-milliseconds, fine enough to make
// Telescope-style "user typed another char, kill the in-flight search"
// responsive without paying the cost of checking inside the per-line loop.
pub static CANCEL: AtomicBool = AtomicBool::new(false);

const DEFAULT_TOP_N: usize = 20;

fn main() {
    install_sigpipe_default();
    install_cancel_handlers();

    let Args { needle, top_n, limits } = match parse_args() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("usage: fuz [-n N] [--no-file-limit] [--no-line-limit] PATTERN");
            std::process::exit(2);
        }
    };

    let matcher = Matcher::new(&needle);
    let topk = Arc::new(TopK::new(top_n));
    let profile = std::env::var_os("FUZ_PROFILE").is_some();

    let _watchdog = Watchdog::spawn();

    let t0 = std::time::Instant::now();
    walker::run(matcher, Arc::clone(&topk), &STATS, limits);
    let wall = t0.elapsed();

    // Cancellation: walker may have exited early because a signal arrived or
    // the consumer closed stdout. Emit nothing in either case — partial
    // top-K is not the user-visible ranking and writing to a closed pipe
    // would SIGPIPE us mid-flush anyway.
    if CANCEL.load(Ordering::Relaxed) {
        std::process::exit(130);
    }

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

struct Args {
    needle: String,
    top_n: usize,
    limits: Limits,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut needle: Option<String> = None;
    let mut top_n: usize = DEFAULT_TOP_N;
    let mut max_file_size: Option<u64> = Some(DEFAULT_MAX_FILE_SIZE);
    let mut max_line_len: Option<usize> = Some(DEFAULT_MAX_LINE_LEN);
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
        } else if a == "--no-file-limit" {
            max_file_size = None;
        } else if a == "--no-line-limit" {
            max_line_len = None;
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
    Ok(Args {
        needle,
        top_n,
        limits: Limits { max_file_size, max_line_len },
    })
}

#[cfg(unix)]
fn install_sigpipe_default() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn install_sigpipe_default() {}

#[cfg(unix)]
extern "C" fn handle_cancel_signal(_: libc::c_int) {
    // _exit is async-signal-safe and bypasses destructors / atexit handlers.
    // We use it instead of merely flipping a flag because the in-progress
    // match_score() call on a single huge "line" (minified JS, JSON, lockfile)
    // can run for hundreds of ms without ever returning to a point where a
    // flag check could fire. Partial top-K state is meaningless on cancel —
    // there's nothing worth flushing.
    unsafe { libc::_exit(130) };
}

#[cfg(unix)]
fn install_cancel_handlers() {
    let handler = handle_cancel_signal as *const () as libc::sighandler_t;
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

#[cfg(not(unix))]
fn install_cancel_handlers() {}

/// Watches stdout for hangup so that consumers (e.g. Telescope) which cancel
/// by closing the read end of the pipe get a prompt reaction. Without this,
/// `fuz` would not notice — output is fully buffered until after the walker
/// finishes, so SIGPIPE never fires during the search.
///
/// Implementation: poll() fd 1 with events=0 (POLLHUP/POLLERR are reported
/// regardless of the events mask) plus a self-pipe so main can wake the
/// thread instantly when the search completes normally. Idle cost is zero —
/// the thread sleeps inside poll().
#[cfg(unix)]
struct Watchdog {
    wake_wr: libc::c_int,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl Watchdog {
    fn spawn() -> Option<Self> {
        let mut fds: [libc::c_int; 2] = [0; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc != 0 {
            return None;
        }
        let wake_rd = fds[0];
        let wake_wr = fds[1];

        let handle = std::thread::spawn(move || {
            loop {
                let mut pfds = [
                    libc::pollfd { fd: 1, events: 0, revents: 0 },
                    libc::pollfd { fd: wake_rd, events: libc::POLLIN, revents: 0 },
                ];
                let r = unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) };
                if r < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    break;
                }
                if pfds[1].revents != 0 {
                    break;
                }
                if pfds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    // Same reasoning as the signal handler: don't try to flag-and-poll
                    // our way out of a deep match_score on a giant "line". Terminate
                    // the process directly. The consumer (Telescope) closed the pipe,
                    // so there's nothing to flush.
                    unsafe { libc::_exit(130) };
                }
            }
            unsafe { libc::close(wake_rd) };
        });

        Some(Self { wake_wr, handle: Some(handle) })
    }
}

#[cfg(unix)]
impl Drop for Watchdog {
    fn drop(&mut self) {
        let byte: u8 = 0;
        unsafe {
            libc::write(self.wake_wr, &byte as *const u8 as *const libc::c_void, 1);
            libc::close(self.wake_wr);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(not(unix))]
struct Watchdog;

#[cfg(not(unix))]
impl Watchdog {
    fn spawn() -> Option<Self> { None }
}
