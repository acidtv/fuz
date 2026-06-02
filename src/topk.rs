use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

#[derive(Eq, PartialEq)]
pub struct Scored {
    pub score: i32,
    pub line_no: u64,
    pub prefix: Box<[u8]>, // b"path:" pre-baked
    pub line: Box<[u8]>,
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Order primarily by score (higher = "greater"), break ties by (path, line_no)
        // so that ties are deterministic.
        self.score
            .cmp(&other.score)
            .then_with(|| self.prefix.cmp(&other.prefix))
            .then_with(|| self.line_no.cmp(&other.line_no))
    }
}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

pub struct TopK {
    k: usize,
    heap: Mutex<BinaryHeap<Reverse<Scored>>>,
    /// Current "worst kept" score. Reads are lock-free and let most candidates
    /// short-circuit without taking the mutex.
    cutoff: AtomicI32,
}

impl TopK {
    pub fn new(k: usize) -> Self {
        TopK {
            k,
            heap: Mutex::new(BinaryHeap::with_capacity(k.min(1024))),
            cutoff: AtomicI32::new(i32::MIN),
        }
    }

    /// Try to insert a match. Fast-path returns immediately if `score` can't
    /// beat the current cutoff. Slow-path acquires the mutex, allocates the
    /// owned bytes, and updates the cutoff if the heap is now full.
    #[inline]
    pub fn try_insert(&self, score: i32, prefix: &[u8], line_no: u64, line: &[u8]) {
        if score <= self.cutoff.load(Ordering::Relaxed) {
            return;
        }
        let mut heap = self.heap.lock().unwrap();
        if heap.len() < self.k {
            heap.push(Reverse(Scored {
                score,
                line_no,
                prefix: prefix.into(),
                line: line.into(),
            }));
            if heap.len() == self.k {
                self.cutoff
                    .store(heap.peek().unwrap().0.score, Ordering::Relaxed);
            }
        } else if score > heap.peek().unwrap().0.score {
            heap.pop();
            heap.push(Reverse(Scored {
                score,
                line_no,
                prefix: prefix.into(),
                line: line.into(),
            }));
            self.cutoff
                .store(heap.peek().unwrap().0.score, Ordering::Relaxed);
        }
    }

    /// Drain the heap into a vec sorted best-first (highest score first).
    pub fn into_sorted_best_first(self) -> Vec<Scored> {
        let heap = self.heap.into_inner().unwrap();
        // BinaryHeap<Reverse<T>>::into_sorted_vec sorts by Reverse<T> ascending,
        // i.e. by T descending — exactly best-first.
        heap.into_sorted_vec().into_iter().map(|r| r.0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(score: i32) -> Scored {
        Scored {
            score,
            line_no: 0,
            prefix: Box::from(&b"p:"[..]),
            line: Box::from(&b"line"[..]),
        }
    }

    #[test]
    fn keeps_top_k() {
        let t = TopK::new(3);
        for s in [10, 5, 7, 2, 9, 1, 8] {
            t.try_insert(s, b"p:", 0, b"x");
        }
        let v: Vec<i32> = t.into_sorted_best_first().iter().map(|s| s.score).collect();
        assert_eq!(v, vec![10, 9, 8]);
    }

    #[test]
    fn cutoff_filters_below() {
        let t = TopK::new(2);
        t.try_insert(10, b"p:", 0, b"x");
        t.try_insert(20, b"p:", 0, b"x");
        // Heap is full, cutoff = 10
        assert_eq!(t.cutoff.load(Ordering::Relaxed), 10);
        t.try_insert(5, b"p:", 0, b"x"); // below cutoff, ignored
        t.try_insert(10, b"p:", 0, b"x"); // equals cutoff, not strictly greater, ignored
        let v: Vec<i32> = t.into_sorted_best_first().iter().map(|s| s.score).collect();
        assert_eq!(v, vec![20, 10]);
    }

    #[test]
    fn fewer_than_k_returns_all() {
        let t = TopK::new(10);
        t.try_insert(3, b"p:", 0, b"x");
        t.try_insert(1, b"p:", 0, b"x");
        t.try_insert(2, b"p:", 0, b"x");
        let v: Vec<i32> = t.into_sorted_best_first().iter().map(|s| s.score).collect();
        assert_eq!(v, vec![3, 2, 1]);
    }

    #[test]
    fn ord_breaks_ties_deterministically() {
        let a = scored(5);
        let mut b = scored(5);
        b.line_no = 1;
        assert!(b > a);
    }
}
