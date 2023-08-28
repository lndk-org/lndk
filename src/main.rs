mod clock;
mod lnd;
mod onion_messenger;
mod rate_limit;
mod internal {
    #![allow(clippy::enum_variant_names)]
    #![allow(clippy::unnecessary_lazy_evaluations)]
    #![allow(clippy::useless_conversion)]
    #![allow(clippy::never_loop)]
    #![allow(clippy::uninlined_format_args)]

    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

#[macro_use]
extern crate configure_me;

use crate::lnd::{
    features_support_onion_messages, get_lnd_client, string_to_network, LndCfg, LndNodeSigner,
};

use crate::onion_messenger::{run_onion_messenger, MessengerUtilities};
use bitcoin::secp256k1::PublicKey;
use internal::*;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::onion_message::{DefaultMessageRouter, OnionMessenger};
use log::{error, info};
use std::collections::HashMap;
use std::str::FromStr;
use tonic_lnd::lnrpc::GetInfoRequest;

#[tokio::main]
async fn main() -> Result<(), ()> {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let lnd_config = Config::including_optional_config_files(&["./lndk.conf"])
        .unwrap_or_exit()
        .0;

    let args = LndCfg::new(lnd_config.address, lnd_config.cert, lnd_config.macaroon);
    let mut client = get_lnd_client(args).expect("failed to connect");

    let info = client
        .lightning()
        .get_info(GetInfoRequest {})
        .await
        .expect("failed to get info")
        .into_inner();

    let mut network_str = None;
    for chain in info.chains {
        if chain.chain == "bitcoin" {
            network_str = Some(chain.network.clone())
        }
    }
    if network_str.is_none() {
        error!("lnd node is not connected to bitcoin network as expected");
        return Err(());
    }
    let network = string_to_network(&network_str.unwrap());

    let pubkey = PublicKey::from_str(&info.identity_pubkey).unwrap();
    info!("Starting lndk for node: {pubkey}.");

    if !features_support_onion_messages(&info.features) {
        error!("LND must support onion messaging to run LNDK.");
        return Err(());
    }

    // On startup, we want to get a list of our currently online peers to notify the onion messenger that they are
    // connected. This sets up our "start state" for the messenger correctly.
    let current_peers = client
        .lightning()
        .list_peers(tonic_lnd::lnrpc::ListPeersRequest {
            latest_error: false,
        })
        .await
        .map_err(|e| {
            error!("Could not lookup current peers: {e}.");
        })?;

    let mut peer_support = HashMap::new();
    for peer in current_peers.into_inner().peers {
        let pubkey = PublicKey::from_str(&peer.pub_key).unwrap();
        let onion_support = features_support_onion_messages(&peer.features);
        peer_support.insert(pubkey, onion_support);
    }

    // Create an onion messenger that depends on LND's signer client and consume related events.
    let mut node_client = client.signer().clone();
    let node_signer = LndNodeSigner::new(pubkey, &mut node_client);
    let messenger_utils = MessengerUtilities::new();
    let onion_messenger = OnionMessenger::new(
        &messenger_utils,
        &node_signer,
        &messenger_utils,
        &DefaultMessageRouter {},
        IgnoringMessageHandler {},
        IgnoringMessageHandler {},
    );

    let mut peers_client = client.lightning().clone();
    run_onion_messenger(
        peer_support,
        &mut peers_client,
        onion_messenger,
        network.unwrap(),
    )
    .await
}

#[cfg(test)]
mod tests {
    pub mod test_utils;
}
