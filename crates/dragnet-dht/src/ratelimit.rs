// SPDX-License-Identifier: AGPL-3.0-only
//! Basit token-bucket rate limiter.
//!
//! Giden DHT sorgularının hızını sınırlamak için kullanılır: ağı sel altında
//! bırakmamak ve kendi kaynaklarımızı korumak için. Kilit altında tutulacak kadar
//! ucuzdur; her `try_take` çağrısında geçen süreye göre kova doldurulur.

use std::time::Instant;

/// Saniyede `refill_per_sec` jeton üreten, en fazla `capacity` jeton biriktiren kova.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    /// `rate` = saniyedeki jeton (aynı zamanda patlama kapasitesi).
    pub fn new(rate: f64) -> Self {
        let rate = rate.max(1.0);
        Self {
            capacity: rate,
            tokens: rate,
            refill_per_sec: rate,
            last: Instant::now(),
        }
    }

    /// Bir jeton varsa tüketip `true`, yoksa `false` döner.
    pub fn try_take(&mut self) -> bool {
        self.try_take_at(Instant::now())
    }

    /// Test edilebilirlik için saati dışarıdan alan varyant.
    pub fn try_take_at(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
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
    fn burst_then_throttle() {
        let start = Instant::now();
        let mut tb = TokenBucket::new(10.0);
        // Baştaki patlama: kapasitedeki tüm jetonlar harcanabilir.
        for _ in 0..10 {
            assert!(tb.try_take_at(start));
        }
        // Kova boşaldı.
        assert!(!tb.try_take_at(start));
    }

    #[test]
    fn refills_over_time() {
        let start = Instant::now();
        let mut tb = TokenBucket::new(10.0);
        for _ in 0..10 {
            assert!(tb.try_take_at(start));
        }
        // 500 ms sonra ~5 jeton dolmalı.
        let later = start + Duration::from_millis(500);
        let mut granted = 0;
        for _ in 0..10 {
            if tb.try_take_at(later) {
                granted += 1;
            }
        }
        assert!((4..=6).contains(&granted), "granted={granted}");
    }
}
