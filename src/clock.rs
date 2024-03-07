use tokio::time::Instant;

pub trait Clock {
    fn now(&self) -> Instant;
}

/// TokioClock provides an implementation of the Clock trait using Tokio's time module.
pub struct TokioClock;

impl TokioClock {
    pub fn new() -> Self {
        TokioClock
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}
