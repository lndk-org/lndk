use bitcoin::secp256k1::PublicKey;
use std::collections::HashMap;

// CurrentPeers keeps up-to-date with all of the peers we're connected and disconnected to using a map.
pub(crate) struct CurrentPeers {
    peer_map: HashMap<PublicKey, bool>,
}

impl CurrentPeers {
    pub(crate) fn new(peers: HashMap<PublicKey, bool>) -> CurrentPeers {
        CurrentPeers { peer_map: peers }
    }

    pub(crate) fn peer_connected(&mut self, peer_key: PublicKey, onion_support: bool) {
        self.peer_map.insert(peer_key, onion_support);
    }

    pub(crate) fn peer_disconnected(&mut self, peer_key: PublicKey) {
        self.peer_map.remove(&peer_key);
    }

    pub(crate) fn peers(&self) -> Vec<PublicKey> {
        self.peer_map.keys().cloned().collect::<Vec<PublicKey>>()
    }
}
