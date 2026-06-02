use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use ignore::{WalkBuilder, WalkState};
use memchr::{memchr, memchr_iter};

use crate::matcher::Matcher;
use crate::stats::Stats;
use crate::topk::TopK;

const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;
const BINARY_PROBE: usize = 8192;
const PREFILTER_THRESHOLD: usize = 64 * 1024;
const BOM: &[u8] = b"\xEF\xBB\xBF";

thread_local! {
    static READ_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(64 * 1024));
    static PREFIX_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(256));
}

pub fn run(matcher: Matcher, topk: Arc<TopK>, stats: &'static Stats) {
    let matcher = Arc::new(matcher);

    let walker = WalkBuilder::new(".")
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .require_git(false)
        .build_parallel();

    walker.run(|| {
        let matcher = Arc::clone(&matcher);
        let topk = Arc::clone(&topk);
        Box::new(move |result| {
            let entry = match result {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            if !entry.file_type().map_or(false, |t| t.is_file()) {
                return WalkState::Continue;
            }
            stats.files_seen.fetch_add(1, Ordering::Relaxed);
            let path = entry.path();
            let t_total = Instant::now();
            search_file(path, &matcher, &topk, stats);
            Stats::add_ns(&stats.ns_total, t_total);
            WalkState::Continue
        })
    });
}

fn search_file(path: &Path, matcher: &Matcher, topk: &TopK, stats: &Stats) {
    READ_BUF.with(|read_cell| {
    PREFIX_BUF.with(|prefix_cell| {
            let mut buf = read_cell.borrow_mut();
            let mut prefix = prefix_cell.borrow_mut();
            buf.clear();

            // === phase: I/O (open + size check via fstat + fadvise + read) ===
            let t_io = Instant::now();
            let mut file = match File::open(path) {
                Ok(f) => f,
                Err(_) => {
                    Stats::add_ns(&stats.ns_io, t_io);
                    return;
                }
            };

            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if len > MAX_FILE_SIZE {
                stats.files_oversize_skip.fetch_add(1, Ordering::Relaxed);
                Stats::add_ns(&stats.ns_io, t_io);
                return;
            }

            #[cfg(target_os = "linux")]
            unsafe {
                use std::os::unix::io::AsRawFd;
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
            }

            buf.reserve(len as usize);
            if file.read_to_end(&mut buf).is_err() {
                Stats::add_ns(&stats.ns_io, t_io);
                return;
            }
            Stats::add_ns(&stats.ns_io, t_io);
            stats.bytes_read.fetch_add(buf.len() as u64, Ordering::Relaxed);

            // === phase: binary detect ===
            let t_bin = Instant::now();
            let probe_end = buf.len().min(BINARY_PROBE);
            let is_binary = memchr(0, &buf[..probe_end]).is_some();
            Stats::add_ns(&stats.ns_binary_check, t_bin);
            if is_binary {
                stats.files_binary_skip.fetch_add(1, Ordering::Relaxed);
                return;
            }

            // === phase: prefilter ===
            if buf.len() > PREFILTER_THRESHOLD {
                let t_pre = Instant::now();
                let passes = matcher.prefilter_passes(&buf);
                Stats::add_ns(&stats.ns_prefilter, t_pre);
                if !passes {
                    stats.files_prefilter_skip.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }

            stats.files_searched.fetch_add(1, Ordering::Relaxed);

            // === phase: search (scan + try_insert into TopK) ===
            let t_search = Instant::now();
            let n_matches = search_buffer(path, &buf, matcher, topk, &mut prefix);
            Stats::add_ns(&stats.ns_search, t_search);
            stats.matches.fetch_add(n_matches, Ordering::Relaxed);
    });
    });
}

fn search_buffer(
    path: &Path,
    buf: &[u8],
    matcher: &Matcher,
    topk: &TopK,
    prefix: &mut Vec<u8>,
) -> u64 {
    let path_bytes = path_to_bytes(path);
    let path_bytes = strip_dot_prefix(path_bytes);

    prefix.clear();
    prefix.extend_from_slice(path_bytes);
    prefix.push(b':');

    let mut line_start = 0usize;
    let mut line_no: u64 = 1;
    let mut first_line = true;
    let mut n_matches: u64 = 0;

    for newline_pos in memchr_iter(b'\n', buf) {
        let mut line_end = newline_pos;
        if line_end > line_start && buf[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        let mut line = &buf[line_start..line_end];
        if first_line && line.starts_with(BOM) {
            line = &line[BOM.len()..];
        }
        if let Some(score) = matcher.match_score(line) {
            topk.try_insert(score, prefix, line_no, line);
            n_matches += 1;
        }
        line_start = newline_pos + 1;
        line_no += 1;
        first_line = false;
    }

    if line_start < buf.len() {
        let mut line = &buf[line_start..];
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if first_line && line.starts_with(BOM) {
            line = &line[BOM.len()..];
        }
        if let Some(score) = matcher.match_score(line) {
            topk.try_insert(score, prefix, line_no, line);
            n_matches += 1;
        }
    }
    n_matches
}

#[cfg(unix)]
fn path_to_bytes(path: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes()
}

#[cfg(not(unix))]
fn path_to_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

fn strip_dot_prefix(p: &[u8]) -> &[u8] {
    if p.starts_with(b"./") {
        &p[2..]
    } else {
        p
    }
}
