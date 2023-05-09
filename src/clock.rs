use tokio::time::Instant;

pub trait Clock {
    fn now(&self) -> Instant;
}

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
