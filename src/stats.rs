use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub struct Stats {
    pub files_seen: AtomicU64,
    pub files_searched: AtomicU64,
    pub files_binary_skip: AtomicU64,
    pub files_prefilter_skip: AtomicU64,
    pub files_oversize_skip: AtomicU64,
    pub bytes_read: AtomicU64,
    pub matches: AtomicU64,
    pub ns_io: AtomicU64,
    pub ns_binary_check: AtomicU64,
    pub ns_prefilter: AtomicU64,
    pub ns_search: AtomicU64,
    pub ns_write: AtomicU64,
    pub ns_total: AtomicU64,
}

impl Stats {
    pub const fn new() -> Self {
        Stats {
            files_seen: AtomicU64::new(0),
            files_searched: AtomicU64::new(0),
            files_binary_skip: AtomicU64::new(0),
            files_prefilter_skip: AtomicU64::new(0),
            files_oversize_skip: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            matches: AtomicU64::new(0),
            ns_io: AtomicU64::new(0),
            ns_binary_check: AtomicU64::new(0),
            ns_prefilter: AtomicU64::new(0),
            ns_search: AtomicU64::new(0),
            ns_write: AtomicU64::new(0),
            ns_total: AtomicU64::new(0),
        }
    }

    #[inline]
    pub fn add_ns(field: &AtomicU64, t: Instant) {
        field.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn print(&self, wall: std::time::Duration) {
        let ns = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64 / 1e6;
        let n = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let wall_ms = wall.as_secs_f64() * 1000.0;
        let cpu_ms = self.ns_total.load(Ordering::Relaxed) as f64 / 1e6;
        let bytes = self.bytes_read.load(Ordering::Relaxed) as f64;
        let mb = bytes / 1_048_576.0;

        eprintln!("--- fuz profile ---");
        eprintln!("wall            : {wall_ms:>9.2} ms");
        eprintln!("cpu (sum thrds) : {cpu_ms:>9.2} ms   (= sum of per-file work across all threads)");
        eprintln!("files seen      : {}", n(&self.files_seen));
        eprintln!("files searched  : {}", n(&self.files_searched));
        eprintln!("  oversize skip : {}", n(&self.files_oversize_skip));
        eprintln!("  binary skip   : {}", n(&self.files_binary_skip));
        eprintln!("  prefilter skip: {}", n(&self.files_prefilter_skip));
        eprintln!("matches written : {}", n(&self.matches));
        eprintln!("bytes read      : {mb:>9.2} MiB");
        eprintln!("throughput      : {:>9.2} MiB/s  (bytes_read / wall)", mb / (wall_ms / 1000.0));
        eprintln!();
        let total = ns(&self.ns_io) + ns(&self.ns_binary_check) + ns(&self.ns_prefilter)
            + ns(&self.ns_search) + ns(&self.ns_write);
        let pct = |x: f64| if total > 0.0 { x / total * 100.0 } else { 0.0 };
        eprintln!("phase breakdown (cpu-time across all threads):");
        eprintln!("  io (open+read+fadvise) : {:>9.2} ms  {:>5.1}%", ns(&self.ns_io), pct(ns(&self.ns_io)));
        eprintln!("  binary detect          : {:>9.2} ms  {:>5.1}%", ns(&self.ns_binary_check), pct(ns(&self.ns_binary_check)));
        eprintln!("  prefilter              : {:>9.2} ms  {:>5.1}%", ns(&self.ns_prefilter), pct(ns(&self.ns_prefilter)));
        eprintln!("  search (scan+format)   : {:>9.2} ms  {:>5.1}%", ns(&self.ns_search), pct(ns(&self.ns_search)));
        eprintln!("  output write           : {:>9.2} ms  {:>5.1}%", ns(&self.ns_write), pct(ns(&self.ns_write)));
        eprintln!("  total accounted        : {total:>9.2} ms");
    }
}
