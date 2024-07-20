mod clock;
#[allow(dead_code)]
pub mod lnd;
pub mod lndk_offers;
pub mod onion_messenger;
mod rate_limit;
pub mod server;

pub mod lndkrpc {
    tonic::include_proto!("lndkrpc");
}

use crate::lnd::{
    features_support_onion_messages, get_lnd_client, get_network, has_build_tags, has_version,
    LndCfg, LndNodeSigner, MIN_LND_MAJOR_VER, MIN_LND_MINOR_VER, MIN_LND_PATCH_VER,
    MIN_LND_PRE_RELEASE_VER,
};
use crate::lndk_offers::{OfferError, SendPaymentParams};
use crate::onion_messenger::{LndkNodeIdLookUp, MessengerUtilities};
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::{PublicKey, Secp256k1};
use home::home_dir;
use lightning::blinded_path::{BlindedPath, Direction, IntroductionNode};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::msgs::DecodeError;
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
use lnd::BUILD_TAGS_REQUIRED;
use log::{debug, error, info, LevelFilter};
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Config as LogConfig, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex, Once};
use tokio::time::{sleep, timeout, Duration};
use tonic_lnd::lnrpc::{ChanInfoRequest, GetInfoRequest, Payment};
use tonic_lnd::verrpc::VersionRequest;
use tonic_lnd::Client;
use triggered::{Listener, Trigger};

static INIT: Once = Once::new();

pub fn init_logger(config: LogConfig) {
    INIT.call_once(|| {
        log4rs::init_config(config).expect("failed to initialize logger");
    });
}

pub const DEFAULT_SERVER_HOST: &str = "127.0.0.1";
pub const DEFAULT_SERVER_PORT: u16 = 7000;
pub const LDK_LOGGER_NAME: &str = "ldk";
pub const DEFAULT_DATA_DIR: &str = ".lndk";

pub const TLS_CERT_FILENAME: &str = "tls-cert.pem";
pub const TLS_KEY_FILENAME: &str = "tls-key.pem";

#[allow(clippy::result_unit_err)]
pub fn setup_logger(log_level: Option<String>, log_dir: Option<String>) -> Result<(), ()> {
    let log_level = match log_level {
        Some(level_str) => match LevelFilter::from_str(&level_str) {
            Ok(level) => level,
            Err(_) => {
                // Since the logger isn't set up yet, we use a println just this once.
                println!(
                    "User provided log level '{}' is invalid. Make sure it is set to either 'error',
                    'warn', 'info', 'debug' or 'trace'",
                    level_str
                );
                return Err(());
            }
        },
        None => LevelFilter::Trace,
    };

    let log_dir = log_dir.unwrap_or_else(|| {
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
        .logger(Logger::builder().build(LDK_LOGGER_NAME, log_level))
        .build(
            Root::builder()
                .appender("stdout")
                .appender("lndk_logs")
                .build(log_level),
        )
        .unwrap();

    init_logger(config);

    Ok(())
}

#[derive(Clone)]
pub struct Cfg {
    pub lnd: LndCfg,
    pub signals: LifecycleSignals,
    pub skip_version_check: bool,
}

#[derive(Clone)]
pub struct LifecycleSignals {
    // Use to externally trigger shutdown.
    pub shutdown: Trigger,
    // Used to listen for the signal to shutdown.
    pub listener: Listener,
}

pub struct LndkOnionMessenger {}

impl LndkOnionMessenger {
    pub fn new() -> Self {
        LndkOnionMessenger {}
    }

    pub async fn run(
        &self,
        args: Cfg,
        offer_handler: Arc<impl OffersMessageHandler>,
    ) -> Result<(), ()> {
        let mut client = get_lnd_client(args.lnd).expect("failed to connect");
        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .expect("failed to get info")
            .into_inner();
        let network = get_network(info.clone()).await?;

        let pubkey = PublicKey::from_str(&info.identity_pubkey).unwrap();
        info!("Starting lndk on {network} network for node: {pubkey}.");

        if !features_support_onion_messages(&info.features) {
            error!("LND must support onion messaging to run LNDK.");
            return Err(());
        }

        let version = client
            .versioner()
            .get_version(VersionRequest {})
            .await
            .expect("failed to get version")
            .into_inner();

        if !has_build_tags(&version, None) {
            error!(
                "LND build tags '{}' are not compatible with LNDK. Make sure '{}' are enabled.",
                &version.build_tags.join(", "),
                BUILD_TAGS_REQUIRED.join(", ")
            );
            return Err(());
        }

        if !args.skip_version_check && !has_version(&version, None) {
            error!(
                    "The LND version {} is not compatible with LNDK. Please update to version {}.{}.{}-{} or higher.",
                    &version.version, MIN_LND_MAJOR_VER, MIN_LND_MINOR_VER, MIN_LND_PATCH_VER, MIN_LND_PRE_RELEASE_VER
                );
            return Err(());
        }

        // On startup, we want to get a list of our currently online peers to notify the onion
        // messenger that they are connected. This sets up our "start state" for the
        // messenger correctly.
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
        let message_router = &DefaultMessageRouter::new(network_graph, &messenger_utils);
        let node_id_lookup = LndkNodeIdLookUp::new(client.clone(), pubkey);
        let onion_messenger = OnionMessenger::new(
            &messenger_utils,
            &node_signer,
            &messenger_utils,
            &node_id_lookup,
            message_router,
            offer_handler,
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

impl Default for LndkOnionMessenger {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
enum PaymentState {
    InvoiceRequestCreated,
    InvoiceRequestSent,
    InvoiceReceived,
    PaymentDispatched,
    Paid,
}

pub struct OfferHandler {
    // active_payments holds a list of payments we're currently attempting to make. When we create
    // a new invoice request for a payment, we set a PaymentId in its metadata, which we also store
    // here. Then we wait until we receive an invoice with the same PaymentId.
    active_payments: Mutex<HashMap<PaymentId, PaymentInfo>>,
    pending_messages: Mutex<Vec<PendingOnionMessage<OffersMessage>>>,
    pub messenger_utils: MessengerUtilities,
    expanded_key: ExpandedKey,
}

pub struct PaymentInfo {
    state: PaymentState,
    invoice: Option<Bolt12Invoice>,
}

#[derive(Clone)]
pub struct PayOfferParams {
    pub offer: Offer,
    pub amount: Option<u64>,
    pub payer_note: Option<String>,
    pub network: Network,
    pub client: Client,
    /// The destination the offer creator provided, which we will use to send the invoice request.
    pub destination: Destination,
    /// The path we will send back to the offer creator, so it knows where to send back the
    /// invoice.
    pub reply_path: Option<BlindedPath>,
}

impl OfferHandler {
    pub fn new() -> Self {
        let messenger_utils = MessengerUtilities::new();
        let random_bytes = messenger_utils.get_secure_random_bytes();
        let expanded_key = ExpandedKey::new(&KeyMaterial(random_bytes));

        OfferHandler {
            active_payments: Mutex::new(HashMap::new()),
            pending_messages: Mutex::new(Vec::new()),
            messenger_utils,
            expanded_key,
        }
    }

    /// Adds an offer to be paid with the amount specified. May only be called once for a single
    /// offer.
    pub async fn pay_offer(&self, cfg: PayOfferParams) -> Result<Payment, OfferError> {
        let client_clone = cfg.client.clone();
        let (invoice, validated_amount, payment_id) = self.get_invoice(cfg).await?;

        self.pay_invoice(client_clone, validated_amount, &invoice, payment_id)
            .await
    }

    /// Sends an invoice request and waits for an invoice to be sent back to us.
    /// Reminder that if this method returns an error after create_invoice_request is called, we
    /// *must* remove the payment_id from self.active_payments.
    pub(crate) async fn get_invoice(
        &self,
        cfg: PayOfferParams,
    ) -> Result<(Bolt12Invoice, u64, PaymentId), OfferError> {
        let (invoice_request, payment_id, validated_amount) = self
            .create_invoice_request(
                cfg.client.clone(),
                cfg.offer.clone(),
                cfg.network,
                cfg.amount,
                cfg.payer_note,
            )
            .await?;

        self.send_invoice_request(
            cfg.destination.clone(),
            cfg.client.clone(),
            cfg.reply_path.clone(),
            invoice_request,
        )
        .await
        .map_err(|e| {
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments.remove(&payment_id);
            e
        })?;

        let invoice =
            match timeout(Duration::from_secs(20), self.wait_for_invoice(payment_id)).await {
                Ok(invoice) => invoice,
                Err(_) => {
                    error!("Did not receive invoice in 100 seconds.");
                    let mut active_payments = self.active_payments.lock().unwrap();
                    active_payments.remove(&payment_id);
                    return Err(OfferError::InvoiceTimeout);
                }
            };
        {
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments
                .entry(payment_id)
                .and_modify(|entry| entry.state = PaymentState::InvoiceReceived);
        }

        Ok((invoice, validated_amount, payment_id))
    }

    /// Sends an invoice request and waits for an invoice to be sent back to us.
    /// Reminder that if this method returns an error after create_invoice_request is called, we
    /// *must* remove the payment_id from self.active_payments.
    pub(crate) async fn pay_invoice(
        &self,
        client: Client,
        amount: u64,
        invoice: &Bolt12Invoice,
        payment_id: PaymentId,
    ) -> Result<Payment, OfferError> {
        let payment_hash = invoice.payment_hash();
        let path_info = invoice.payment_paths()[0].clone();

        let params = SendPaymentParams {
            path: path_info.1,
            cltv_expiry_delta: path_info.0.cltv_expiry_delta,
            fee_base_msat: path_info.0.fee_base_msat,
            fee_ppm: path_info.0.fee_proportional_millionths,
            payment_hash: payment_hash.0,
            msats: amount,
            payment_id,
        };

        let intro_node_id = match params.path.introduction_node {
            IntroductionNode::NodeId(node_id) => Some(node_id.to_string()),
            IntroductionNode::DirectedShortChannelId(direction, scid) => {
                let get_chan_info_request = ChanInfoRequest { chan_id: scid };
                let chan_info = client
                    .clone()
                    .lightning_read_only()
                    .get_chan_info(get_chan_info_request)
                    .await
                    .map_err(OfferError::GetChannelInfo)?
                    .into_inner();
                match direction {
                    Direction::NodeOne => Some(chan_info.node1_pub),
                    Direction::NodeTwo => Some(chan_info.node2_pub),
                }
            }
        };
        debug!(
            "Attempting to pay invoice with introduction node {:?}",
            intro_node_id
        );

        self.send_payment(client, params)
            .await
            .map(|payment| {
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
                payment
            })
            .map_err(|e| {
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
                e
            })
    }

    /// wait_for_invoice waits for the offer creator to respond with an invoice.
    async fn wait_for_invoice(&self, payment_id: PaymentId) -> Bolt12Invoice {
        loop {
            {
                let active_payments = self.active_payments.lock().unwrap();
                if let Some(pay_info) = active_payments.get(&payment_id) {
                    if let Some(invoice) = pay_info.invoice.clone() {
                        return invoice;
                    }
                };
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
                info!("Received an invoice: {invoice:?}");
                let secp_ctx = &Secp256k1::new();
                // We verify that this invoice is a response to an invoice request we sent.
                match invoice.verify(&self.expanded_key, secp_ctx) {
                    Ok(payment_id) => {
                        info!("Successfully verified invoice for payment_id {payment_id}");
                        let mut active_payments = self.active_payments.lock().unwrap();
                        match active_payments.get_mut(&payment_id) {
                            Some(pay_info) => match pay_info.invoice {
                                Some(_) => {
                                    error!("We already received an invoice with this payment id.")
                                }
                                None => {
                                    pay_info.state = PaymentState::InvoiceReceived;
                                    pay_info.invoice = Some(invoice.clone());
                                }
                            },
                            None => {
                                error!("We received an invoice request for a payment id that we don't recognize or already paid: {payment_id:?}. We will ignore the invoice.");
                            }
                        }
                        Some(OffersMessage::Invoice(invoice))
                    }
                    Err(()) => {
                        error!("Invoice verification failed for invoice: {invoice:?}");
                        Some(OffersMessage::InvoiceError(InvoiceError::from_string(
                            String::from("invoice verification failure"),
                        )))
                    }
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

pub struct Bolt12InvoiceString(pub String);

impl TryFrom<Bolt12InvoiceString> for Bolt12Invoice {
    type Error = DecodeError;

    fn try_from(s: Bolt12InvoiceString) -> Result<Self, Self::Error> {
        let bytes: Vec<u8> = hex::decode(s.0).unwrap();
        Self::try_from(bytes).map_err(|_| DecodeError::InvalidValue)
    }
}

impl From<String> for Bolt12InvoiceString {
    fn from(s: String) -> Self {
        Bolt12InvoiceString(s)
    }
}

#[cfg(test)]
mod tests {
    pub mod test_utils;
}
