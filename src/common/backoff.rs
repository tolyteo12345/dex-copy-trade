use std::time::Duration;

/// Exponential backoff without jitter. Intentionally simple: one bot instance per RPC
/// endpoint, so thundering-herd jitter isn't needed; add jitter if many instances share a node.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            current: initial,
        }
    }

    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_until_cap_then_resets() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_millis(500));
        let delays: Vec<u128> = (0..5).map(|_| b.next_delay().as_millis()).collect();
        assert_eq!(delays, vec![100, 200, 400, 500, 500]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(100));
    }
}
