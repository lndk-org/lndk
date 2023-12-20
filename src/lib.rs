mod clock;
#[allow(dead_code)]
pub mod lnd;
pub mod lndk_offers;
mod onion_messenger;
mod rate_limit;

use crate::lnd::{
    features_support_onion_messages, get_lnd_client, string_to_network, LndCfg, LndNodeSigner,
};

use crate::onion_messenger::{run_onion_messenger, MessengerUtilities};
use bitcoin::secp256k1::PublicKey;
use home::home_dir;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::offers::offer::Offer;
use lightning::onion_message::{
    DefaultMessageRouter, OffersMessage, OffersMessageHandler, OnionMessenger, PendingOnionMessage,
};
use log::{error, info, LevelFilter};
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Config as LogConfig, Root};
use log4rs::encode::pattern::PatternEncoder;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use tonic_lnd::lnrpc::GetInfoRequest;
use triggered::{Listener, Trigger};

pub struct Cfg {
    pub lnd: LndCfg,
    pub log_dir: Option<String>,
    // Use to externally trigger shutdown.
    pub shutdown: Trigger,
    // Used to listen for the signal to shutdown.
    pub listener: Listener,
}

pub enum OfferError {
    OfferAlreadyAdded,
}

enum OfferState {
    OfferAdded,
    InvoiceRequestSent,
    InvoiceReceived,
    InvoicePaymentDispatched,
    InvoicePaid,
}

pub struct OfferHandler {
    active_offers: Mutex<HashMap<Offer, OfferState>>,
    pending_messages: Mutex<Vec<PendingOnionMessage<OffersMessage>>>,
}

impl OfferHandler {
    pub fn new() -> Self {
        OfferHandler {
            active_offers: Mutex::new(HashMap::new()),
            pending_messages: Mutex::new(Vec::new()),
        }
    }

    /// Adds an offer to be paid with the amount specified. May only be called once for a single offer.
    pub fn pay_offer(&mut self, _offer: Offer, _amount: u64) -> Result<(), OfferError> {
        /*
           Check if we've already added offer -> error.
           Add offer to state map.
           Create invoice request and push to offer queue.
        */
        Ok(())
    }

    pub async fn run(&self, args: Cfg) -> Result<(), ()> {
        let log_dir = args.log_dir.unwrap_or_else(|| {
            home_dir()
                .unwrap()
                .join(".lndk")
                .join("lndk.log")
                .as_path()
                .to_str()
                .unwrap()
                .to_string()
        });

        // Log both to stdout and a log file.
        let stdout = ConsoleAppender::builder().build();
        let lndk_logs = FileAppender::builder()
            .encoder(Box::new(PatternEncoder::new("{d} - {m}{n}")))
            .build(log_dir)
            .unwrap();

        let config = LogConfig::builder()
            .appender(Appender::builder().build("stdout", Box::new(stdout)))
            .appender(Appender::builder().build("lndk_logs", Box::new(lndk_logs)))
            .build(
                Root::builder()
                    .appender("stdout")
                    .appender("lndk_logs")
                    .build(LevelFilter::Info),
            )
            .unwrap();

        let _log_handle = log4rs::init_config(config).unwrap();

        let mut client = get_lnd_client(args.lnd).expect("failed to connect");

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
            self,
            IgnoringMessageHandler {},
        );

        let mut peers_client = client.lightning().clone();
        run_onion_messenger(
            peer_support,
            &mut peers_client,
            onion_messenger,
            network.unwrap(),
            args.shutdown,
            args.listener,
        )
        .await
    }
}

impl OffersMessageHandler for OfferHandler {
    fn handle_message(&self, message: OffersMessage) -> Option<OffersMessage> {
        match message {
            OffersMessage::InvoiceRequest(_) => {
                log::error!("Invoice request received, payment not yet supported.");
                None
            }
            OffersMessage::Invoice(_invoice) => {
                // lookup corresponding invoice request / fail if not known
                // Validate invoice for invoice request
                // Progress state to invoice received
                // Dispatch payment and update state
                None
            }
            OffersMessage::InvoiceError(_error) => None,
        }
    }

    fn release_pending_messages(&self) -> Vec<PendingOnionMessage<OffersMessage>> {
        core::mem::take(&mut self.pending_messages.lock().unwrap())
    }
}

impl Default for OfferHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    pub mod test_utils;
}
