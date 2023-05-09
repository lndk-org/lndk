use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;

// TokenLimiter keeps a map of currently online peers.
pub(crate) struct TokenLimiter {
    peer_map: HashMap<PublicKey, bool>,
}

impl TokenLimiter {
    pub(crate) fn new(peers: HashMap<PublicKey, bool>) -> TokenLimiter {
        TokenLimiter { peer_map: peers }
    }

    pub(crate) fn peer_connected(&mut self, peer_key: PublicKey) {
        self.peer_map.insert(peer_key, true);
    }

    pub(crate) fn peer_disconnected(&mut self, peer_key: PublicKey) {
        self.peer_map.remove(&peer_key);
    }

    pub(crate) fn peers(&self) -> Vec<PublicKey> {
        self.peer_map.keys().cloned().collect::<Vec<PublicKey>>()
    }
}
