use crate::clock::Clock;
use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;
use std::marker::Copy;
use tokio::time::{Duration, Instant};

/// Peer record holds information about a peer that we are (or have been) connected to to.
#[derive(Copy, Clone)]
struct PeerRecord {
    online: bool,
    remaining_calls: u8,
}

impl PeerRecord {
    fn new(online: bool, remaining_calls: u8) -> Self {
        PeerRecord {
            online,
            remaining_calls,
        }
    }
}

pub(crate) trait RateLimiter {
    fn peer_connected(&mut self, peer_key: PublicKey);
    fn peer_disconnected(&mut self, peer_key: PublicKey);
    fn peers(&self) -> Vec<PublicKey>;
    fn query_peer(&mut self, peer_key: PublicKey) -> bool;
}

// TokenLimiter keeps a map of currently online peers.
pub(crate) struct TokenLimiter<C: Clock> {
    peer_map: HashMap<PublicKey, PeerRecord>,
    clock: C,
    call_count: u8,
    call_frequency: Duration,
    last_update: Instant,
}

impl<C: Clock> TokenLimiter<C> {
    pub(crate) fn new(
        peers: impl Iterator<Item = PublicKey>,
        call_count: u8,
        call_frequency: Duration,
        clock: C,
    ) -> Self {
        let mut peer_map: HashMap<PublicKey, PeerRecord> = HashMap::new();
        for peer in peers {
            peer_map.insert(peer, PeerRecord::new(true, call_count));
        }

        let last_update = clock.now();
        Self {
            peer_map,
            clock,
            call_count,
            call_frequency,
            last_update,
        }
    }

    fn needs_update(&self) -> bool {
        self.clock.now().duration_since(self.last_update) >= self.call_frequency
    }

    fn update(&mut self) {
        // We can safely delete offline peers because they would have their call count updated at
        // this point anyway. We want to delete so that we don't allow an infinitely growing queue.
        self.peer_map.retain(|_, v| v.online);

        // Refresh allowed call counts per-peer that's left online.
        for (_, v) in self.peer_map.iter_mut() {
            v.remaining_calls = self.call_count;
        }

        self.last_update = self.clock.now();
    }

    fn hit(&mut self, peer_key: PublicKey) -> bool {
        match self.peer_map.get_mut(&peer_key) {
            Some(v) => {
                if v.remaining_calls == 0 {
                    return false;
                }

                v.remaining_calls -= 1;
                true
            }
            None => false,
        }
    }
}

impl<C: Clock> RateLimiter for TokenLimiter<C> {
    fn peer_connected(&mut self, peer_key: PublicKey) {
        self.peer_map
            .entry(peer_key)
            .and_modify(|e| e.online = true)
            .or_insert(PeerRecord::new(true, self.call_count));
    }

    fn peer_disconnected(&mut self, peer_key: PublicKey) {
        self.peer_map
            .entry(peer_key)
            .and_modify(|e| e.online = false);
    }

    fn peers(&self) -> Vec<PublicKey> {
        self.peer_map
            .clone()
            .into_iter()
            .filter(|p| p.1.online)
            .map(|p| p.0)
            .collect::<Vec<PublicKey>>()
    }

    fn query_peer(&mut self, peer_key: PublicKey) -> bool {
        if self.needs_update() {
            self.update();
        };

        self.hit(peer_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_utils::pubkey;
    use core::ops::SubAssign;
    use mockall::mock;
    use tokio::time::{Duration, Instant};

    const TEST_COUNT: u8 = 2;
    const TEST_FREQUENCY: Duration = Duration::from_secs(1);

    mock! {
        FixedClock{}

        impl Clock for FixedClock{
            fn now(&self) -> Instant;
        }
    }

    #[test]
    fn test_peer_connection() {
        let pk_0 = pubkey(0);
        let pk_1 = pubkey(1);

        let mut clock = MockFixedClock::new();
        let start_time = Instant::now();
        clock.expect_now().returning(move || start_time);

        // Assert that we're set up with our original peer.
        let mut rate_limiter =
            TokenLimiter::new(vec![pk_0].into_iter(), TEST_COUNT, TEST_FREQUENCY, clock);
        assert_eq!(rate_limiter.peers(), vec![pk_0]);

        // Connect a new peer and assert that both are reported.
        rate_limiter.peer_connected(pk_1);
        assert_eq!(rate_limiter.peers().sort(), vec![pk_0, pk_1].sort());

        // Disconnect our original peer and assert that it's no longer listed.
        rate_limiter.peer_disconnected(pk_0);
        assert_eq!(rate_limiter.peers(), vec![pk_1]);
    }

    #[test]
    fn test_rate_limiting() {
        let pk_0 = pubkey(0);
        let pk_1 = pubkey(1);

        let mut clock = MockFixedClock::new();

        // TODO: use constant value for mocking.
        let start_time = Instant::now();
        clock.expect_now().returning(move || start_time);

        // Assert that we're set up with our original peer.
        let mut rate_limiter =
            TokenLimiter::new(vec![pk_0].into_iter(), TEST_COUNT, TEST_FREQUENCY, clock);
        assert_eq!(rate_limiter.peers(), vec![pk_0]);

        // Exhaust our allowed call count in this period, then assert that we're no longer allowed to query further.
        for _ in 0..TEST_COUNT {
            assert!(rate_limiter.query_peer(pk_0));
        }
        assert!(!rate_limiter.query_peer(pk_0));

        // Connect a new peer, use some calls, then flip our connection off and on and assert that they aren't allowed
        // more calls by virtue of having disconnected.
        rate_limiter.peer_connected(pk_1);
        assert!(rate_limiter.query_peer(pk_1));

        rate_limiter.peer_disconnected(pk_1);
        rate_limiter.peer_connected(pk_1);

        assert!(rate_limiter.query_peer(pk_1));
        assert!(!rate_limiter.query_peer(pk_1));

        // Disconnect one peer so that they'll be cleaned up and update the rate limiter's time so that we'll refresh
        // our buckets.
        rate_limiter.peer_disconnected(pk_1);

        // Update our clock to a time which reflects that we need an update.
        rate_limiter.last_update.sub_assign(TEST_FREQUENCY);

        // The disconnected peer should not be allowed any queries (they're currently unknown to us).
        assert!(!rate_limiter.query_peer(pk_1));

        // Our original peer should be allowed the full quota of peers again.
        for _ in 0..TEST_COUNT {
            assert!(rate_limiter.query_peer(pk_0));
        }
        assert!(!rate_limiter.query_peer(pk_0));

        // When we reconnect a previously disconnected peer, they should once again have access to the full quota of
        // calls.
        rate_limiter.peer_connected(pk_1);
        for _ in 0..TEST_COUNT {
            assert!(rate_limiter.query_peer(pk_1));
        }

        assert!(!rate_limiter.query_peer(pk_1));
    }

    #[test]
    fn test_query_peer_unknown_peer() {
        let pk_0 = pubkey(0);

        let mut clock = MockFixedClock::new();
        let start_time = Instant::now();
        clock.expect_now().returning(move || start_time);

        let mut rate_limiter =
            TokenLimiter::new(vec![].into_iter(), TEST_COUNT, TEST_FREQUENCY, clock);

        assert!(!rate_limiter.query_peer(pk_0));
    }
}
