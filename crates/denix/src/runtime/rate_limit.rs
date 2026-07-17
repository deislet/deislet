use std::time::Instant;

/// Simple token bucket rate limiter
pub struct RateLimiter {
    tokens: f64,
    last_refill: Instant,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
}

impl RateLimiter {
    pub fn new(rate: f64) -> Self {
        Self {
            tokens: rate,
            last_refill: Instant::now(),
            max_tokens: rate,
            refill_rate: rate,
        }
    }

    pub fn check(&mut self) -> bool {
        self.check_at(Instant::now())
    }

    /// Internal method: Check at specific time, for testing
    pub fn check_at(&mut self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();

        // Refill
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_rate_limiter_basic() {
        let mut limiter = RateLimiter::new(10.0); // 10 ops/sec
        let start = Instant::now();

        // Should initially have 10 tokens
        for _ in 0..10 {
            assert!(limiter.check_at(start));
        }

        // The 11th should fail
        assert!(!limiter.check_at(start));
    }

    #[test]
    fn test_rate_limiter_refill() {
        let mut limiter = RateLimiter::new(10.0);
        let start = Instant::now();

        // Consume all
        for _ in 0..10 {
            limiter.check_at(start);
        }
        assert!(!limiter.check_at(start));

        // After 0.1 seconds, should refill 1 token (10 * 0.1 = 1)
        let later = start + Duration::from_millis(100);
        assert!(limiter.check_at(later));

        // Exhaust again
        assert!(!limiter.check_at(later));
    }

    #[test]
    fn test_rate_limiter_overflow() {
        let mut limiter = RateLimiter::new(10.0);
        let start = Instant::now();

        // After a long time, tokens should not exceed max_tokens (10)
        let later = start + Duration::from_secs(100);

        // Can only consume 10 times
        for _ in 0..10 {
            assert!(limiter.check_at(later));
        }
        assert!(!limiter.check_at(later));
    }
}
