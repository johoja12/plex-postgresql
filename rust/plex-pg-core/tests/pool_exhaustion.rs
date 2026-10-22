use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct Pool {
    slots: Mutex<Vec<bool>>, // true = in use
}

impl Pool {
    fn new(size: usize) -> Self {
        Self {
            slots: Mutex::new(vec![false; size]),
        }
    }

    fn acquire(&self, timeout: Duration) -> Option<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut slots = self.slots.lock().unwrap();
                if let Some((idx, slot)) = slots.iter_mut().enumerate().find(|(_, s)| !**s) {
                    *slot = true;
                    return Some(idx);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn release(&self, idx: usize) {
        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(idx) {
            *slot = false;
        }
    }
}

#[test]
fn pool_exhaustion_times_out_under_pressure() {
    let pool = Arc::new(Pool::new(2));
    let timeouts = Arc::new(AtomicUsize::new(0));

    // Keep both slots occupied until the contenders have finished. The old
    // sleep-only test could schedule the short-lived contenders first and
    // observe no exhaustion at all on a busy build host.
    let ready = Arc::new(Barrier::new(3));
    let release = Arc::new(Barrier::new(3));
    let mut holders = Vec::new();
    for _ in 0..2 {
        let pool = pool.clone();
        let ready = ready.clone();
        let release = release.clone();
        holders.push(thread::spawn(move || {
            let idx = pool.acquire(Duration::from_millis(30)).unwrap();
            ready.wait();
            release.wait();
            pool.release(idx);
        }));
    }
    ready.wait();
    let mut contenders = Vec::new();
    for _ in 0..2 {
        let pool = pool.clone();
        let timeouts = timeouts.clone();
        contenders.push(thread::spawn(move || {
            if let Some(idx) = pool.acquire(Duration::from_millis(30)) {
                pool.release(idx);
            } else {
                timeouts.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in contenders {
        h.join().unwrap();
    }
    release.wait();
    for h in holders {
        h.join().unwrap();
    }

    let total_timeouts = timeouts.load(Ordering::Relaxed);
    assert!(
        total_timeouts == 2,
        "expected at least one timeout under exhaustion"
    );
}
