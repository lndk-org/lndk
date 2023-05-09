use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;

pub(crate) trait RateLimiter {
    fn peer_connected(&mut self, peer_key: PublicKey);
    fn peer_disconnected(&mut self, peer_key: PublicKey);
    fn peers(&self) -> Vec<PublicKey>;
}

// TokenLimiter keeps a map of currently online peers.
pub(crate) struct TokenLimiter {
    peer_map: HashMap<PublicKey, bool>,
}

impl TokenLimiter {
    pub(crate) fn new(peers: HashMap<PublicKey, bool>) -> TokenLimiter {
        TokenLimiter { peer_map: peers }
    }
}

impl RateLimiter for TokenLimiter {
    fn peer_connected(&mut self, peer_key: PublicKey) {
        self.peer_map.insert(peer_key, true);
    }

    fn peer_disconnected(&mut self, peer_key: PublicKey) {
        self.peer_map.remove(&peer_key);
    }

    fn peers(&self) -> Vec<PublicKey> {
        self.peer_map.keys().cloned().collect::<Vec<PublicKey>>()
    }
}
