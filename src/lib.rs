mod clock;
#[allow(dead_code)]
pub mod lnd;
pub mod lndk_offers;
pub mod onion_messenger;
mod rate_limit;

use crate::lnd::{
    features_support_onion_messages, get_lnd_client, string_to_network, LndCfg, LndNodeSigner,
};
use crate::lndk_offers::{OfferError, PayInvoiceParams};
use crate::onion_messenger::MessengerUtilities;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::{Error as Secp256k1Error, PublicKey, Secp256k1};
use home::home_dir;
use lightning::blinded_path::BlindedPath;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::invoice_error::InvoiceError;
use lightning::offers::offer::Offer;
use lightning::onion_message::messenger::{
    DefaultMessageRouter, Destination, OnionMessenger, PendingOnionMessage,
};
use lightning::onion_message::offers::{OffersMessage, OffersMessageHandler};
use lightning::routing::gossip::NetworkGraph;
use lightning::sign::{EntropySource, KeyMaterial};
use log::{error, info, LevelFilter};
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Config as LogConfig, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, Once};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{sleep, timeout, Duration};
use tonic_lnd::lnrpc::GetInfoRequest;
use tonic_lnd::Client;
use triggered::{Listener, Trigger};

static INIT: Once = Once::new();

pub struct Cfg {
    pub lnd: LndCfg,
    pub log_dir: Option<String>,
    pub log_level: LevelFilter,
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
            .logger(Logger::builder().build("h2", LevelFilter::Info))
            .logger(Logger::builder().build("hyper", LevelFilter::Info))
            .logger(Logger::builder().build("rustls", LevelFilter::Info))
            .logger(Logger::builder().build("tokio_util", LevelFilter::Info))
            .logger(Logger::builder().build("tracing", LevelFilter::Info))
            .build(
                Root::builder()
                    .appender("stdout")
                    .appender("lndk_logs")
                    .build(args.log_level),
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
        #[allow(deprecated)]
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
        let network = network.unwrap();

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
        let network_graph = &NetworkGraph::new(network, &messenger_utils);
        let message_router = &DefaultMessageRouter::new(network_graph);
        let onion_messenger = OnionMessenger::new(
            &messenger_utils,
            &node_signer,
            &messenger_utils,
            message_router,
            &self.offer_handler,
            IgnoringMessageHandler {},
        );

        let mut peers_client = client.lightning().clone();
        self.run_onion_messenger(
            peer_support,
            &mut peers_client,
            onion_messenger,
            network,
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
    active_offers: Mutex<HashMap<String, OfferState>>,
    active_invoices: Mutex<Vec<Bolt12Invoice>>,
    pending_messages: Mutex<Vec<PendingOnionMessage<OffersMessage>>>,
    pub messenger_utils: MessengerUtilities,
    expanded_key: ExpandedKey,
}

#[derive(Clone)]
pub struct PayOfferParams {
    pub offer: Offer,
    pub amount: Option<u64>,
    pub network: Network,
    pub client: Client,
    /// The destination the offer creator provided, which we will use to send the invoice request.
    pub destination: Destination,
    /// The path we will send back to the offer creator, so it knows where to send back the invoice.
    pub reply_path: Option<BlindedPath>,
}

impl OfferHandler {
    pub fn new() -> Self {
        let messenger_utils = MessengerUtilities::new();
        let random_bytes = messenger_utils.get_secure_random_bytes();
        let expanded_key = ExpandedKey::new(&KeyMaterial(random_bytes));

        OfferHandler {
            active_offers: Mutex::new(HashMap::new()),
            active_invoices: Mutex::new(Vec::new()),
            pending_messages: Mutex::new(Vec::new()),
            messenger_utils,
            expanded_key,
        }
    }

    /// Adds an offer to be paid with the amount specified. May only be called once for a single offer.
    pub async fn pay_offer(
        &self,
        cfg: PayOfferParams,
        started: Receiver<u32>,
    ) -> Result<(), OfferError<Secp256k1Error>> {
        let client_clone = cfg.client.clone();
        let offer_id = cfg.offer.clone().to_string();
        let validated_amount = self.send_invoice_request(cfg, started).await?;

        let invoice = match timeout(Duration::from_secs(100), self.wait_for_invoice()).await {
            Ok(invoice) => invoice,
            Err(_) => {
                error!("Did not receive invoice in 100 seconds.");
                return Err(OfferError::InvoiceTimeout);
            }
        };
        {
            let mut active_offers = self.active_offers.lock().unwrap();
            active_offers.insert(offer_id.clone(), OfferState::InvoiceReceived);
        }

        let payment_hash = invoice.payment_hash();
        let path_info = invoice.payment_paths()[0].clone();

        let params = PayInvoiceParams {
            path: path_info.1,
            cltv_expiry_delta: path_info.0.cltv_expiry_delta,
            fee_base_msat: path_info.0.fee_base_msat,
            fee_ppm: path_info.0.fee_proportional_millionths,
            payment_hash: payment_hash.0,
            msats: validated_amount,
            offer_id,
        };

        self.pay_invoice(client_clone, params).await
    }

    /// wait_for_invoice waits for the offer creator to respond with an invoice.
    async fn wait_for_invoice(&self) -> Bolt12Invoice {
        loop {
            {
                let mut active_invoices = self.active_invoices.lock().unwrap();
                if active_invoices.len() == 1 {
                    return active_invoices.pop().unwrap();
                }
            }
            sleep(Duration::from_secs(2)).await;
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
            OffersMessage::Invoice(invoice) => {
                let secp_ctx = &Secp256k1::new();
                // We verify that this invoice is a response to the invoice request we just sent.
                match invoice.verify(&self.expanded_key, secp_ctx) {
                    // TODO: Eventually when we allow for multiple payments in flight, we can use the
                    // returned payment id below to check if we already processed an invoice for
                    // this payment. Right now it's safe to let this be because we won't try to pay
                    // a second invoice (if it comes through).
                    Ok(_payment_id) => {
                        let mut active_invoices = self.active_invoices.lock().unwrap();
                        active_invoices.push(invoice.clone());
                        Some(OffersMessage::Invoice(invoice))
                    }
                    Err(()) => Some(OffersMessage::InvoiceError(InvoiceError::from_string(
                        String::from("invoice verification failure"),
                    ))),
                }
            }
            OffersMessage::InvoiceError(error) => {
                log::error!("Invoice error received: {}", error);
                None
            }
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
