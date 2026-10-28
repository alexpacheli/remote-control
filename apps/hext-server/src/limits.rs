//! Rate limits: at most `max` events per `window` for each key.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter<K> {
    max: usize,
    window: Duration,
    hits: Mutex<HashMap<K, VecDeque<Instant>>>,
}

impl<K: Eq + Hash + Clone> RateLimiter<K> {
    pub fn new(max: usize, window: Duration) -> Self {
        Self {
            max,
            window,
            hits: Mutex::default(),
        }
    }

    /// Records an event for `key` if it is within the limit; false if not.
    pub fn allow(&self, key: &K, now: Instant) -> bool {
        let mut hits = self.hits.lock().expect("lock");
        let times = hits.entry(key.clone()).or_default();
        self.forget_old(times, now);
        if times.len() >= self.max {
            return false;
        }
        times.push_back(now);
        true
    }

    /// Drops keys with no recent events (call now and then: memory).
    pub fn prune(&self, now: Instant) {
        let mut hits = self.hits.lock().expect("lock");
        hits.retain(|_, times| {
            self.forget_old(times, now);
            !times.is_empty()
        });
    }

    fn forget_old(&self, times: &mut VecDeque<Instant>, now: Instant) {
        while times
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= self.window)
        {
            times.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_each_key_within_the_window() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        let t = Instant::now();
        assert!(limiter.allow(&"a", t));
        assert!(limiter.allow(&"a", t));
        assert!(!limiter.allow(&"a", t));
        assert!(limiter.allow(&"b", t), "keys are separate");
        // Refused attempts do not count.
        assert!(limiter.allow(&"a", t + Duration::from_secs(60)));
    }

    #[test]
    fn prune_forgets_idle_keys() {
        let limiter = RateLimiter::new(1, Duration::from_secs(1));
        let t = Instant::now();
        limiter.allow(&1, t);
        limiter.prune(t + Duration::from_secs(2));
        assert!(limiter.hits.lock().unwrap().is_empty());
    }
}
