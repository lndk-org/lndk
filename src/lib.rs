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
use bitcoin::secp256k1::{PublicKey, Secp256k1};
use bitcoin::Network;
use home::home_dir;
use lightning::blinded_path::message::{BlindedMessagePath, OffersContext};
use lightning::blinded_path::{Direction, IntroductionNode};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::ln::msgs::DecodeError;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::invoice_error::InvoiceError;
use lightning::offers::offer::Offer;
use lightning::onion_message::messenger::{
    DefaultMessageRouter, Destination, MessageSendInstructions, OnionMessenger, Responder,
    ResponseInstruction,
};
use lightning::onion_message::offers::{OffersMessage, OffersMessageHandler};
use lightning::routing::gossip::NetworkGraph;
use lightning::sign::EntropySource;
use lnd::BUILD_TAGS_REQUIRED;
use log::{debug, error, info, trace, warn, LevelFilter};
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Config as LogConfig, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;
use rate_limit::RateLimiterCfg;
use std::collections::HashMap;
use std::path::PathBuf;
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
pub const DEFAULT_DATA_DIR: &str = "data";
pub const DEFAULT_LNDK_DIR: &str = ".lndk";
pub const DEFAULT_LOG_FILE: &str = "lndk.log";
pub const DEFAULT_CONFIG_FILE_NAME: &str = "lndk.conf";

pub const TLS_CERT_FILENAME: &str = "tls-cert.pem";
pub const TLS_KEY_FILENAME: &str = "tls-key.pem";
pub const DEFAULT_RESPONSE_INVOICE_TIMEOUT: u32 = 15;

#[allow(clippy::result_unit_err)]
pub fn setup_logger(log_level: Option<String>, log_file: Option<PathBuf>) -> Result<(), ()> {
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

    let log_file = log_file.unwrap_or_else(|| {
        home_dir()
            .unwrap()
            .join(DEFAULT_LNDK_DIR)
            .join(DEFAULT_DATA_DIR)
            .join(DEFAULT_LOG_FILE)
    });

    // Log both to stdout and a log file.
    let stdout = ConsoleAppender::builder().build();
    let lndk_logs = FileAppender::builder()
        .encoder(Box::new(PatternEncoder::new("{d} - {m}{n}")))
        .build(log_file)
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
    pub rate_limit_count: u8,
    pub rate_limit_period_secs: u64,
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
        let messenger_utils = MessengerUtilities::default();
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
            IgnoringMessageHandler {}, // AsyncPaymentsMessageHandler
            IgnoringMessageHandler {}, // DNSResolverMessageHandler
            IgnoringMessageHandler {}, // CustomOnionMessageHandler
        );

        let mut peers_client = client.lightning().clone();
        self.run_onion_messenger(
            peer_support,
            &mut peers_client,
            onion_messenger,
            network,
            args.signals,
            RateLimiterCfg {
                call_count: args.rate_limit_count,
                call_period_secs: Duration::from_secs(args.rate_limit_period_secs),
            },
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
    pending_messages: Mutex<Vec<(OffersMessage, MessageSendInstructions)>>,
    pub messenger_utils: MessengerUtilities,
    expanded_key: ExpandedKey,
    /// The amount of time in seconds that we will wait for the offer creator to respond with
    /// an invoice. If not provided, we will use the default value of 15 seconds.
    pub response_invoice_timeout: u32,
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
    pub reply_path: Option<BlindedMessagePath>,
    /// The amount of time in seconds that we will wait for the offer creator to respond with
    /// an invoice. If not provided, we will use the default value of 15 seconds.
    pub response_invoice_timeout: Option<u32>,
}

impl OfferHandler {
    pub fn new(response_invoice_timeout: Option<u32>, seed: Option<[u8; 32]>) -> Self {
        let messenger_utils = MessengerUtilities::default();
        let random_bytes = match seed {
            Some(seed) => seed,
            None => messenger_utils.get_secure_random_bytes(),
        };
        let expanded_key = ExpandedKey::new(random_bytes);
        let response_invoice_timeout =
            response_invoice_timeout.unwrap_or(DEFAULT_RESPONSE_INVOICE_TIMEOUT);

        OfferHandler {
            active_payments: Mutex::new(HashMap::new()),
            pending_messages: Mutex::new(Vec::new()),
            messenger_utils,
            expanded_key,
            response_invoice_timeout,
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
        let (invoice_request, payment_id, validated_amount, offer_context) = self
            .create_invoice_request(cfg.offer.clone(), cfg.network, cfg.amount, cfg.payer_note)
            .await?;

        self.send_invoice_request(
            cfg.destination.clone(),
            cfg.client.clone(),
            invoice_request,
            offer_context,
        )
        .await
        .inspect_err(|_| {
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments.remove(&payment_id);
        })?;

        let cfg_timeout = cfg
            .response_invoice_timeout
            .unwrap_or(self.response_invoice_timeout);

        let invoice = match timeout(
            Duration::from_secs(cfg_timeout as u64),
            self.wait_for_invoice(payment_id),
        )
        .await
        {
            Ok(invoice) => invoice,
            Err(_) => {
                error!("Did not receive invoice in {cfg_timeout} seconds.");
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
                return Err(OfferError::InvoiceTimeout(cfg_timeout));
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
        let payment_path = &invoice.payment_paths()[0];

        let params = SendPaymentParams {
            path: payment_path.clone(),
            cltv_expiry_delta: payment_path.payinfo.cltv_expiry_delta,
            fee_base_msat: payment_path.payinfo.fee_base_msat,
            fee_ppm: payment_path.payinfo.fee_proportional_millionths,
            payment_hash: payment_hash.0,
            msats: amount,
            payment_id,
        };

        let intro_node_id = match params.path.introduction_node() {
            IntroductionNode::NodeId(node_id) => Some(node_id.to_string()),
            IntroductionNode::DirectedShortChannelId(direction, scid) => {
                let get_chan_info_request = ChanInfoRequest {
                    chan_id: *scid,
                    chan_point: "".to_string(),
                };
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
            .inspect(|_| {
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
            })
            .inspect_err(|_| {
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
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
        Self::new(None, None)
    }
}

impl OffersMessageHandler for OfferHandler {
    fn handle_message(
        &self,
        message: OffersMessage,
        context: Option<OffersContext>,
        responder: Option<Responder>,
    ) -> Option<(OffersMessage, ResponseInstruction)> {
        match message {
            OffersMessage::InvoiceRequest(_) => None,
            OffersMessage::Invoice(invoice) => {
                let secp_ctx = &Secp256k1::new();
                let offer_context = context?;
                let (payment_id, nonce) = match offer_context {
                    OffersContext::OutboundPayment {
                        nonce, payment_id, ..
                    } => (payment_id, nonce),
                    _ => {
                        return None;
                    }
                };
                match invoice.verify_using_payer_data(
                    payment_id,
                    nonce,
                    &self.expanded_key,
                    secp_ctx,
                ) {
                    Ok(payment_id) => {
                        info!("Successfully verified invoice for payment_id {payment_id}");
                        let mut active_payments = self.active_payments.lock().unwrap();
                        let Some(pay_info) = active_payments.get_mut(&payment_id) else {
                            warn!("We received an invoice for a payment that does not exist: {payment_id:?}. Invoice is ignored.");
                            return None;
                        };
                        if pay_info.invoice.is_some() {
                            warn!("We already received an invoice with this payment id. Invoice is ignored.");
                            return None;
                        }
                        pay_info.state = PaymentState::InvoiceReceived;
                        pay_info.invoice = Some(invoice.clone());

                        None
                    }
                    Err(()) => responder.map(|r| {
                        (
                            OffersMessage::InvoiceError(InvoiceError::from_string(String::from(
                                "invoice verification failure",
                            ))),
                            r.respond(),
                        )
                    }),
                }
            }
            OffersMessage::InvoiceError(error) => {
                trace!("Received an invoice error: {error}.");
                if let Some(OffersContext::OutboundPayment {
                    payment_id,
                    nonce,
                    hmac: Some(hmac),
                }) = context
                {
                    self.handle_invoice_error(payment_id, nonce, hmac)
                }
                None
            }
        }
    }

    fn release_pending_messages(&self) -> Vec<(OffersMessage, MessageSendInstructions)> {
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
