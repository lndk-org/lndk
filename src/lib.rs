mod clock;
#[allow(dead_code)]
pub mod lnd;
pub mod lndk_offers;
pub mod onion_messenger;
mod rate_limit;

use crate::lnd::{
    features_support_onion_messages, get_lnd_client, string_to_network, LndCfg, LndNodeSigner,
};

use crate::onion_messenger::MessengerUtilities;
use bitcoin::secp256k1::PublicKey;
use home::home_dir;
use lightning::ln::peer_handler::IgnoringMessageHandler;
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
use std::sync::{Mutex, Once};
use tokio::sync::mpsc::Sender;
use tonic_lnd::lnrpc::GetInfoRequest;
use triggered::{Listener, Trigger};

static INIT: Once = Once::new();

pub struct Cfg {
    pub lnd: LndCfg,
    pub log_dir: Option<String>,
    pub signals: LifecycleSignals,
}

pub struct LifecycleSignals {
    // Use to externally trigger shutdown.
    pub shutdown: Trigger,
    // Used to listen for the signal to shutdown.
    pub listener: Listener,
    // Used to signal when the onion messenger has started up.
    pub started: Sender<u32>,
}

pub fn init_logger(config: LogConfig) {
    INIT.call_once(|| {
        log4rs::init_config(config).expect("failed to initialize logger");
    });
}

pub struct LndkOnionMessenger {
    pub offer_handler: OfferHandler,
}

impl LndkOnionMessenger {
    pub fn new(offer_handler: OfferHandler) -> Self {
        LndkOnionMessenger { offer_handler }
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

        init_logger(config);

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
            &self.offer_handler,
            IgnoringMessageHandler {},
        );

        let mut peers_client = client.lightning().clone();
        self.run_onion_messenger(
            peer_support,
            &mut peers_client,
            onion_messenger,
            network.unwrap(),
            args.signals,
        )
        .await
    }
}

#[allow(dead_code)]
enum OfferState {
    OfferAdded,
    InvoiceRequestSent,
    InvoiceReceived,
    InvoicePaymentDispatched,
    InvoicePaid,
}

pub struct OfferHandler {
    _active_offers: Mutex<HashMap<String, OfferState>>,
    pending_messages: Mutex<Vec<PendingOnionMessage<OffersMessage>>>,
    messenger_utils: MessengerUtilities,
}

impl OfferHandler {
    pub fn new() -> Self {
        OfferHandler {
            _active_offers: Mutex::new(HashMap::new()),
            pending_messages: Mutex::new(Vec::new()),
            messenger_utils: MessengerUtilities::new(),
        }
    }
}

impl Default for OfferHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl OffersMessageHandler for OfferHandler {
    fn handle_message(&self, message: OffersMessage) -> Option<OffersMessage> {
        match message {
            OffersMessage::InvoiceRequest(_) => {
                log::error!("Invoice request received, payment not yet supported.");
                None
            }
            OffersMessage::Invoice(_invoice) => None,
            OffersMessage::InvoiceError(_error) => None,
        }
    }

    fn release_pending_messages(&self) -> Vec<PendingOnionMessage<OffersMessage>> {
        core::mem::take(&mut self.pending_messages.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    pub mod test_utils;
}
