use futures::executor::block_on;
use std::collections::HashMap;
use std::path::PathBuf;
use tonic_lnd::{Client, ConnectError};

const ONION_MESSAGES_REQUIRED: u32 = 38;
pub(crate) const ONION_MESSAGES_OPTIONAL: u32 = 39;

pub(crate) fn get_lnd_client(cfg: LndCfg) -> Result<Client, ConnectError> {
    block_on(tonic_lnd::connect(cfg.address, cfg.cert, cfg.macaroon))
}

pub(crate) struct LndCfg {
    address: String,
    cert: PathBuf,
    macaroon: PathBuf,
}

impl LndCfg {
    pub(crate) fn new(address: String, cert: PathBuf, macaroon: PathBuf) -> LndCfg {
        LndCfg {
            address,
            cert,
            macaroon,
        }
    }
}

// features_support_onion_messages returns a boolean indicating whether a feature set supports onion messaging.
pub(crate) fn features_support_onion_messages(
    features: &HashMap<u32, tonic_lnd::lnrpc::Feature>,
) -> bool {
    features.contains_key(&ONION_MESSAGES_OPTIONAL)
        || features.contains_key(&ONION_MESSAGES_REQUIRED)
}
