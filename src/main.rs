mod clock;
mod lnd;
mod lnd_compatibility;
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

use crate::lnd::{features_support_onion_messages, get_lnd_client, LndCfg, LndNodeSigner};
use crate::onion_messenger::{run_onion_messenger, MessengerUtilities};
use bitcoin::secp256k1::PublicKey;
use internal::*;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::onion_message::OnionMessenger;
use lnd_compatibility::VersionInfo;
use log::{error, info};
use std::collections::HashMap;
use std::str::FromStr;
use tonic_lnd::lnrpc::GetInfoRequest;
use tonic_lnd::verrpc::VersionRequest;

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

    let pubkey = PublicKey::from_str(&info.identity_pubkey).unwrap();
    info!("Starting lndk for node: {pubkey}.");

    if !features_support_onion_messages(&info.features) {
        error!("LND must support onion messaging to run LNDK.");
        return Err(());
    }

    let version_info = client
        .versioner()
        .get_version(VersionRequest {})
        .await
        .expect("failed to get version")
        .into_inner();

    let version_args = VersionInfo {
        version: version_info.version,
        app_minor: version_info.app_minor,
        app_patch: version_info.app_patch,
        build_tags: version_info.build_tags,
    };

    let startup_checks: Result<i32, String> =
        lnd_compatibility::Compatibility::check_compatibility(version_args).await;

    match startup_checks {
        Ok(_) => info!("Running version of LND is compatible with LNDK"),
        Err(error) => error!("{error}"),
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
        IgnoringMessageHandler {},
    );

    let mut peers_client = client.lightning().clone();
    run_onion_messenger(peer_support, &mut peers_client, onion_messenger).await
}

#[cfg(test)]
mod tests {
    pub mod test_utils;
}
