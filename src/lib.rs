mod clock;
mod grpc;
#[allow(dead_code)]
pub mod lnd;
pub mod offers;
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
use crate::onion_messenger::{LndkNodeIdLookUp, MessengerUtilities};
use bitcoin::secp256k1::PublicKey;
use home::home_dir;
use lightning::ln::msgs::DecodeError;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::onion_message::messenger::{DefaultMessageRouter, OnionMessenger};
use lightning::onion_message::offers::OffersMessageHandler;
use lightning::routing::gossip::NetworkGraph;
use lnd::BUILD_TAGS_REQUIRED;
use log::{error, info, LevelFilter};
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::{Appender, Config as LogConfig, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;
use offers::handler::OfferHandler;
use rate_limit::RateLimiterCfg;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Once};
use tokio::time::Duration;
use tonic_lnd::lnrpc::GetInfoRequest;
use tonic_lnd::verrpc::VersionRequest;
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
