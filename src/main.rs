use async_trait::async_trait;
use bitcoin::bech32::u5;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{self, PublicKey, Scalar, Secp256k1};
use futures::executor::block_on;
use lightning::chain::keysinterface::{EntropySource, KeyMaterial, NodeSigner, Recipient};
use lightning::ln::features::InitFeatures;
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::ln::msgs::{Init, OnionMessageHandler};
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::onion_message::{CustomOnionMessageHandler, OnionMessenger};
use lightning::util::logger::{Level, Logger, Record};
use log::{debug, error, info, trace, warn};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::marker::Copy;
use std::ops::Deref;
use std::str::FromStr;
use tokio::select;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tonic_lnd::{
    lnrpc::peer_event::EventType::PeerOffline, lnrpc::peer_event::EventType::PeerOnline,
    lnrpc::GetInfoRequest, lnrpc::GetInfoResponse, lnrpc::NodeInfoRequest, lnrpc::PeerEvent,
    tonic::Code, tonic::Status, Client, ConnectError, LightningClient, PeersClient,
};

const ONION_MESSAGES_REQUIRED: u32 = 38;
const ONION_MESSAGES_OPTIONAL: u32 = 39;

#[tokio::main]
async fn main() -> Result<(), ()> {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let args = match parse_args() {
        Ok(args) => args,
        Err(args) => panic!("Bad arguments: {args}"),
    };

    let mut client = get_lnd_client(args).expect("failed to connect");

    let info = client
        .lightning()
        .get_info(GetInfoRequest {})
        .await
        .expect("failed to get info");
    let info_inner = info.into_inner().clone();

    let pubkey = PublicKey::from_str(&info_inner.identity_pubkey).unwrap();
    info!("Starting lndk for node: {pubkey}");

    if !info_inner.features.contains_key(&ONION_MESSAGES_OPTIONAL) {
        info!("Attempting to set onion messaging feature bit...");

        let mut node_info_retriever = GetInfoClient {
            client: &mut client.lightning().clone(),
        };
        let mut announcement_updater = UpdateNodeAnnClient {
            peers_client: client.peers(),
        };
        match set_feature_bit(&mut node_info_retriever, &mut announcement_updater).await {
            Ok(_) => {}
            Err(err) => {
                error!("error setting feaure bit: {err}");
                return Err(());
            }
        }
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
            error!("Could not lookup current peers: {e}");
        })?;

    let mut peer_support = HashMap::new();
    for peer in current_peers.into_inner().peers {
        let pubkey = PublicKey::from_str(&peer.pub_key).unwrap();
        let onion_support = lookup_onion_support(&pubkey, client.lightning()).await;
        peer_support.insert(pubkey, onion_support);
    }

    // Create an onion messenger that depends on LND's signer client and consume related events.
    let node_signer = LndNodeSigner::new(pubkey, client.signer());
    let messenger_utils = MessengerUtilities::new();
    let onion_messenger = OnionMessenger::new(
        &messenger_utils,
        &node_signer,
        &messenger_utils,
        IgnoringMessageHandler {},
    );

    run_onion_messenger(peer_support, onion_messenger).await
}

// Responsible for initializing the onion messenger provided with the correct start state and managing onion message
// event producers and consumers.
async fn run_onion_messenger<ES: Deref, NS: Deref, L: Deref, CMH: Deref>(
    current_peers: HashMap<PublicKey, bool>,
    onion_messenger: OnionMessenger<ES, NS, L, CMH>,
) -> Result<(), ()>
where
    ES::Target: EntropySource,
    NS::Target: NodeSigner,
    L::Target: Logger,
    CMH::Target: CustomOnionMessageHandler + Sized,
{
    // Setup channels that we'll use to communicate onion messenger events. We buffer our channels by the number of
    // peers that the node currently has so that we can send all of our startup online events in one go (before we
    // boot up the consumer). The number of peers that we have is also related to the number of events we can expect
    // to process, so it's a sensible enough buffer size.
    let (sender, receiver) = channel(current_peers.len());
    for (peer, onion_support) in current_peers {
        sender
            .send(MessengerEvents::PeerConnected(peer, onion_support))
            .await
            .map_err(|e| {
                error!("Notify peer connected: {e}");
            })?
    }

    // Consume events is our main controlling loop, so we run it inline here. We use a RefCell in onion_messenger to
    // allow interior mutibility (see LndNodeSigner) so this function can't safely be passed off to another thread.
    consume_messenger_events(onion_messenger, receiver)
        .await
        .map_err(|e| {
            error!("run onion messenger: {e}");
        })
}

// lookup_onion_support performs a best-effort lookup in the node's view of the graph to determine whether it supports
// onion messaging. If the node is not found, or we have not seen its announcement yet, a warning is logged and we
// assume that onion messaging is not supported.
async fn lookup_onion_support(pubkey: &PublicKey, client: &mut tonic_lnd::LightningClient) -> bool {
    match client
        .get_node_info(NodeInfoRequest {
            pub_key: pubkey.to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(peer) => match peer.into_inner().node {
            Some(node) => {
                node.features.contains_key(&ONION_MESSAGES_OPTIONAL)
                    || node.features.contains_key(&ONION_MESSAGES_REQUIRED)
            }
            None => {
                warn!("Peer {pubkey} not found in graph on startup, assuming no onion support");
                false
            }
        },
        Err(e) => {
            warn!(
                "Could not lookup peer {pubkey} in graph: {e}, assuming no onion message support"
            );
            false
        }
    }
}

#[derive(Debug)]
enum ProducerError {
    SendError(String),
    StreamError(String),
}

impl Error for ProducerError {}

impl fmt::Display for ProducerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ProducerError::SendError(s) => write!(f, "error sending messenger event: {s}"),
            ProducerError::StreamError(s) => write!(f, "LND stream {s} exited"),
        }
    }
}

#[async_trait]
trait PeerEventProducer {
    async fn receive(&mut self) -> Result<PeerEvent, Status>;
    async fn onion_support(&mut self, pubkey: &PublicKey) -> bool;
}

struct PeerStream {
    peer_subscription: tonic_lnd::tonic::Streaming<PeerEvent>,
    client: tonic_lnd::LightningClient,
}

#[async_trait]
impl PeerEventProducer for PeerStream {
    async fn receive(&mut self) -> Result<PeerEvent, Status> {
        match self.peer_subscription.message().await? {
            Some(peer_event) => Ok(peer_event),
            None => Err(Status::unknown("no event provided")),
        }
    }

    async fn onion_support(&mut self, pubkey: &PublicKey) -> bool {
        lookup_onion_support(pubkey, &mut self.client).await
    }
}

// Consumes a stream of peer online/offline events from the PeerEventProducer until the stream exits (by sending an
// error) or the producer receives the signal to exit (via close of the exit channel).
//
// Note that this function *must* send an exit error to the Sender provided on all exit-cases, so that upstream
// consumers know to exit as well. Failures related to sending events are an exception, as failure to send indicates
// that the consumer has already exited (the receiving end of the channel has hung up), and we can't send any more
// events anyway.
async fn produce_peer_events(
    mut source: impl PeerEventProducer,
    events: Sender<MessengerEvents>,
    mut exit: Receiver<()>,
) -> Result<(), ProducerError> {
    loop {
        select! (
            // Select biased so that we'll always check our exit signal before attempting to receive. This allows more
            // deterministic tests, and ensures that the producer will exit when requested (and won't queue up a series
            // of events that can't be consumed, possibly blocking if the channel buffer is small).
            biased;

            _ = exit.recv() => {
                info!("Peer events received signal to quit");
                return Ok(())
            }
            peer_event = source.receive() => {
                match peer_event {
                    Ok(peer_event) => match peer_event.r#type() {
                        PeerOnline => {
                            let pubkey = PublicKey::from_str(&peer_event.pub_key).unwrap();
                            let onion_support = source.onion_support(&pubkey).await;
                            let event = MessengerEvents::PeerConnected(pubkey, onion_support);
                            match events.send(event).await {
                                Ok(_) => debug!("Peer events sent: {event}"),
                                Err(err) => return Err(ProducerError::SendError(format!("{err}"))),
                            };
                        }
                        PeerOffline => {
                            let event = MessengerEvents::PeerDisconnected(PublicKey::from_str(&peer_event.pub_key).unwrap());
                            match events.send(event).await {
                                Ok(_) => debug!("Peer events sent: {event}"),
                                Err(err) => return Err(ProducerError::SendError(format!("{err}"))),
                            };
                        }
                    },
                    Err(s) => {
                        info!("Peer events receive failed: {s}");

                        let event = MessengerEvents::ProducerExit(ConsumerError::PeerProducerExit);
                        match events.send(event).await {
                            Ok(_) => debug!("Peer events sent: {event}"),
                            Err(err) => error!("Peer events: send producer exit failed: {err}"),
                        }
                        return Err(ProducerError::StreamError(format!("{s}")));
                    }
                };
            }
        );
    }
}

#[derive(Debug, Copy, Clone)]
enum ConsumerError {
    // Internal onion messenger implementation has experienced an error.
    OnionMessengerFailure,

    // The producer responsible for peer connection events has exited.
    PeerProducerExit,
}

impl Error for ConsumerError {}

impl fmt::Display for ConsumerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConsumerError::OnionMessengerFailure => {
                write!(f, "consumer err: onion messenger failure")
            }
            ConsumerError::PeerProducerExit => write!(f, "consumer err: peer producer exit"),
        }
    }
}

// MessengerEvents represents all of the events that are relevant to onion messages.
#[derive(Debug, Copy, Clone)]
enum MessengerEvents {
    PeerConnected(PublicKey, bool),
    PeerDisconnected(PublicKey),
    ProducerExit(ConsumerError),
}

impl fmt::Display for MessengerEvents {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MessengerEvents::PeerConnected(p, o) => {
                write!(
                    f,
                    "messenger event: {p} connected, onion message support: {o}"
                )
            }
            MessengerEvents::PeerDisconnected(p) => write!(f, "messenger event: {p} disconnected"),
            MessengerEvents::ProducerExit(s) => write!(f, "messenger event: {s} exited"),
        }
    }
}

// consume_messenger_events receives a series of events and delivers them to the onion messenger provided.
async fn consume_messenger_events(
    onion_messenger: impl OnionMessageHandler,
    mut events: Receiver<MessengerEvents>,
) -> Result<(), ConsumerError> {
    while let Some(onion_event) = events.recv().await {
        info!("Consume messenger events received: {onion_event}");

        match onion_event {
            MessengerEvents::PeerConnected(pubkey, onion_support) => {
                let init_features = if onion_support {
                    let onion_message_optional: u64 = 1 << ONION_MESSAGES_OPTIONAL;
                    InitFeatures::from_le_bytes(onion_message_optional.to_le_bytes().to_vec())
                } else {
                    InitFeatures::empty()
                };

                onion_messenger
                    .peer_connected(
                        &pubkey,
                        &Init {
                            features: init_features,
                            remote_network_address: None,
                        },
                        false,
                    )
                    .map_err(|_| ConsumerError::OnionMessengerFailure)?
            }
            MessengerEvents::PeerDisconnected(pubkey) => onion_messenger.peer_disconnected(&pubkey),
            MessengerEvents::ProducerExit(e) => {
                return Err(e);
            }
        }
    }

    Ok(())
}

struct LndNodeSigner<'a> {
    pubkey: PublicKey,
    secp_ctx: Secp256k1<secp256k1::All>,
    signer: RefCell<&'a mut tonic_lnd::SignerClient>,
}

impl<'a> LndNodeSigner<'a> {
    fn new(pubkey: PublicKey, signer: &'a mut tonic_lnd::SignerClient) -> Self {
        LndNodeSigner {
            pubkey,
            secp_ctx: Secp256k1::new(),
            signer: RefCell::new(signer),
        }
    }
}

impl<'a> NodeSigner for LndNodeSigner<'a> {
    /// Get node id based on the provided [`Recipient`].
    ///
    /// This method must return the same value each time it is called with a given [`Recipient`]
    /// parameter.
    ///
    /// Errors if the [`Recipient`] variant is not supported by the implementation.
    fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
        match recipient {
            Recipient::Node => Ok(self.pubkey),
            Recipient::PhantomNode => Err(()),
        }
    }

    /// Gets the ECDH shared secret of our node secret and `other_key`, multiplying by `tweak` if
    /// one is provided. Note that this tweak can be applied to `other_key` instead of our node
    /// secret, though this is less efficient.
    ///
    /// Errors if the [`Recipient`] variant is not supported by the implementation.
    fn ecdh(
        &self,
        recipient: Recipient,
        other_key: &PublicKey,
        tweak: Option<&Scalar>,
    ) -> Result<SharedSecret, ()> {
        match recipient {
            Recipient::Node => {}
            Recipient::PhantomNode => return Err(()),
        }

        // Clone other_key so that we can tweak it (if a tweak is required). We choose to tweak the
        // `other_key` because LND's API accept a tweak parameter (so we can't tweak our secret).
        let tweaked_key = if let Some(tweak) = tweak {
            other_key.mul_tweak(&self.secp_ctx, tweak).map_err(|_| ())?
        } else {
            *other_key
        };

        let shared_secret = match block_on(self.signer.borrow_mut().derive_shared_key(
            tonic_lnd::signrpc::SharedKeyRequest {
                ephemeral_pubkey: tweaked_key.serialize().into_iter().collect::<Vec<u8>>(),
                key_desc: None,
                ..Default::default()
            },
        )) {
            Ok(shared_key_resp) => shared_key_resp.into_inner().shared_key,
            Err(_) => return Err(()),
        };

        match SharedSecret::from_slice(&shared_secret) {
            Ok(secret) => Ok(secret),
            Err(_) => Err(()),
        }
    }

    fn get_inbound_payment_key_material(&self) -> KeyMaterial {
        unimplemented!("not required for onion messaging");
    }

    fn sign_invoice(
        &self,
        _hrp_bytes: &[u8],
        _invoice_data: &[u5],
        _recipient: Recipient,
    ) -> Result<RecoverableSignature, ()> {
        unimplemented!("not required for onion messaging");
    }

    fn sign_gossip_message(&self, _msg: UnsignedGossipMessage) -> Result<Signature, ()> {
        unimplemented!("not required for onion messaging");
    }
}

// MessengerUtilities implements some utilites required for onion messenging.
struct MessengerUtilities {
    entropy_source: RefCell<ChaCha20Rng>,
}

impl MessengerUtilities {
    fn new() -> Self {
        MessengerUtilities {
            entropy_source: RefCell::new(ChaCha20Rng::from_entropy()),
        }
    }
}

impl EntropySource for MessengerUtilities {
    // TODO: surface LDK's EntropySource and use instead.
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        let mut chacha_bytes: [u8; 32] = [0; 32];
        self.entropy_source
            .borrow_mut()
            .fill_bytes(&mut chacha_bytes);
        chacha_bytes
    }
}

impl Logger for MessengerUtilities {
    fn log(&self, record: &Record) {
        let args_str = record.args.to_string();
        match record.level {
            Level::Gossip => {}
            Level::Trace => trace!("{}", args_str),
            Level::Debug => debug!("{}", args_str),
            Level::Info => info!("{}", args_str),
            Level::Warn => warn!("{}", args_str),
            Level::Error => error!("{}", args_str),
        }
    }
}

fn get_lnd_client(cfg: LndCfg) -> Result<Client, ConnectError> {
    block_on(tonic_lnd::connect(cfg.address, cfg.cert, cfg.macaroon))
}

#[derive(Debug)]
enum ArgsError {
    NoArgs,
    AddressRequired,
    CertRequired,
    MacaroonRequired,
}

impl Error for ArgsError {}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ArgsError::NoArgs => write!(f, "No command line arguments provided."),
            ArgsError::AddressRequired => write!(f, "LND's RPC server address is required."),
            ArgsError::CertRequired => write!(f, "Path to LND's tls certificate is required."),
            ArgsError::MacaroonRequired => write!(f, "Path to LND's macaroon is required."),
        }
    }
}

struct LndCfg {
    address: String,
    cert: String,
    macaroon: String,
}

impl LndCfg {
    fn new(address: String, cert: String, macaroon: String) -> LndCfg {
        LndCfg {
            address,
            cert,
            macaroon,
        }
    }
}

fn parse_args() -> Result<LndCfg, ArgsError> {
    let mut args = std::env::args_os();
    if args.next().is_none() {
        return Err(ArgsError::NoArgs);
    }

    let address = match args.next() {
        Some(arg) => arg.into_string().expect("address is not UTF-8"),
        None => return Err(ArgsError::AddressRequired),
    };

    let cert_file = match args.next() {
        Some(arg) => arg.into_string().expect("cert is not UTF-8"),
        None => return Err(ArgsError::CertRequired),
    };

    let macaroon_file = match args.next() {
        Some(arg) => arg.into_string().expect("macaroon is not UTF-8"),
        None => return Err(ArgsError::MacaroonRequired),
    };

    Ok(LndCfg::new(address, cert_file, macaroon_file))
}

#[derive(Debug)]
enum SetOnionBitError {
    UnimplementedPeersService,
    SetBitFail,
    UpdateAnnouncementErr(Status),
    GetInfoErr(Status),
}

impl Error for SetOnionBitError {}

impl fmt::Display for SetOnionBitError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SetOnionBitError::UnimplementedPeersService => write!(
                f,
                "peer service is unimplemented, remember to
                enable the peerrpc services when building LND with make tags='peerrpc signrpc'"
            ),
            SetOnionBitError::SetBitFail => {
                write!(f, "onion messaging feature bit failed to be set")
            }
            SetOnionBitError::UpdateAnnouncementErr(err) => {
                write!(f, "error setting update announcement: {err}")
            }
            SetOnionBitError::GetInfoErr(err) => write!(f, "get_info error: {err}"),
        }
    }
}

#[async_trait]
trait NodeInfoRetriever {
    async fn get_info(
        &mut self,
        request: GetInfoRequest,
    ) -> Result<GetInfoResponse, SetOnionBitError>;
}

struct GetInfoClient<'a> {
    client: &'a mut LightningClient,
}

#[async_trait]
impl<'a> NodeInfoRetriever for GetInfoClient<'a> {
    async fn get_info(
        &mut self,
        request: GetInfoRequest,
    ) -> Result<GetInfoResponse, SetOnionBitError> {
        match self.client.get_info(request).await {
            Ok(resp) => Ok(resp.into_inner()),
            Err(status) => Err(SetOnionBitError::GetInfoErr(status)),
        }
    }
}

/// UpdateNodeAnnouncement defines the peer functionality needed to set the onion
/// messaging feature bit.
#[async_trait]
trait UpdateNodeAnnouncement {
    async fn update_node_announcement(
        &mut self,
        request: tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest,
    ) -> Result<(), SetOnionBitError>;
}

struct UpdateNodeAnnClient<'a> {
    peers_client: &'a mut PeersClient,
}

#[async_trait]
impl<'a> UpdateNodeAnnouncement for UpdateNodeAnnClient<'a> {
    async fn update_node_announcement(
        &mut self,
        request: tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest,
    ) -> Result<(), SetOnionBitError> {
        match self.peers_client.update_node_announcement(request).await {
            Ok(_) => Ok(()),
            Err(status) => {
                if status.code() == Code::Unimplemented {
                    return Err(SetOnionBitError::UnimplementedPeersService);
                }

                return Err(SetOnionBitError::UpdateAnnouncementErr(status));
            }
        }
    }
}

/// Sets the onion messaging feature bit (described in this PR:
/// https://github.com/lightning/bolts/pull/759/), to signal that we support
/// onion messaging. This needs to be done every time LND starts up, because LND
/// does not currently persist the custom feature bits that are set via the RPC.
async fn set_feature_bit(
    client: &mut impl NodeInfoRetriever,
    peers_client: &mut impl UpdateNodeAnnouncement,
) -> Result<(), SetOnionBitError> {
    let feature_updates = vec![tonic_lnd::peersrpc::UpdateFeatureAction {
        action: i32::from(tonic_lnd::peersrpc::UpdateAction::Add),
        feature_bit: ONION_MESSAGES_OPTIONAL as i32,
    }];

    peers_client
        .update_node_announcement(tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest {
            feature_updates,
            ..Default::default()
        })
        .await?;

    // Call get_info again to check the bit was actually set.
    let info = client.get_info(GetInfoRequest {}).await?;

    if !info.features.contains_key(&ONION_MESSAGES_OPTIONAL) {
        return Err(SetOnionBitError::SetBitFail);
    }

    info!("Successfully set onion messaging bit");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::PublicKey;
    use hex;
    use lightning::ln::features::{InitFeatures, NodeFeatures};
    use lightning::ln::msgs::{OnionMessage, OnionMessageHandler};
    use lightning::util::events::OnionMessageProvider;
    use mockall::mock;
    use std::collections::HashMap;
    use tokio::sync::mpsc::channel;

    mock! {
        InfoRetriever{}

        #[async_trait]
        impl NodeInfoRetriever for InfoRetriever{
            async fn get_info(
                &mut self, request: tonic_lnd::lnrpc::GetInfoRequest,
            ) -> Result<tonic_lnd::lnrpc::GetInfoResponse, SetOnionBitError>;
        }
    }

    mock! {
        NodeAnnouncementUpdater{}

        #[async_trait]
        impl UpdateNodeAnnouncement for NodeAnnouncementUpdater{
            async fn update_node_announcement(
                &mut self, request: tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest,
            ) -> Result<(), SetOnionBitError>;
        }
    }

    #[tokio::test]
    async fn test_set_feature_bit_success() {
        let mut client_mock = MockInfoRetriever::new();
        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();

        // Let's first test that when the peer & main clients return the values
        // we need, set_feature_bit works ok.
        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| Ok(()));

        client_mock.expect_get_info().returning(|_| {
            Ok(GetInfoResponse {
                features: HashMap::from([(
                    ONION_MESSAGES_OPTIONAL,
                    tonic_lnd::lnrpc::Feature {
                        name: String::from("onion_message"),
                        is_known: true,
                        is_required: false,
                    },
                )]),
                ..Default::default()
            })
        });

        let set_feature_bit = set_feature_bit(&mut client_mock, &mut peers_client_mock).await;

        matches!(set_feature_bit, Ok(()));
    }

    #[tokio::test]
    async fn test_update_node_announcement_failure() {
        let mut client_mock = MockInfoRetriever::new();
        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();

        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| {
                Err(SetOnionBitError::UpdateAnnouncementErr(Status::new(
                    Code::Unavailable,
                    "",
                )))
            });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error");

        // If the peers client returns a Status error, set_feature_bit should
        // return that error.
        matches!(set_feature_err, SetOnionBitError::UpdateAnnouncementErr(_));

        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();
        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| {
                Err(SetOnionBitError::UpdateAnnouncementErr(Status::new(
                    Code::Unimplemented,
                    "",
                )))
            });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error with unavailable");

        // If the peers client returns Code::Unimplemented, we should get
        // the correct error message.
        matches!(set_feature_err, SetOnionBitError::UnimplementedPeersService);
    }

    #[tokio::test]
    async fn test_check_if_onion_message_set_failure() {
        let mut client_mock = MockInfoRetriever::new();
        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();

        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| Ok(()));

        client_mock.expect_get_info().returning(|_| {
            Err(SetOnionBitError::UpdateAnnouncementErr(Status::new(
                Code::Unavailable,
                "",
            )))
        });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error with unavailable");

        matches!(set_feature_err, SetOnionBitError::UpdateAnnouncementErr(_));

        // If get_info returns a response with the wrong feature bit set,
        // set_feature_bit should throw another error.
        client_mock.expect_get_info().returning(|_| {
            Ok(GetInfoResponse {
                features: HashMap::from([(
                    8,
                    tonic_lnd::lnrpc::Feature {
                        name: String::from("testing"),
                        is_known: true,
                        is_required: false,
                    },
                )]),
                ..Default::default()
            })
        });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error");

        matches!(set_feature_err, SetOnionBitError::SetBitFail);
    }

    fn pubkey() -> PublicKey {
        PublicKey::from_slice(
            &hex::decode("02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619")
                .unwrap()[..],
        )
        .unwrap()
    }

    mock! {
            OnionHandler{}

            impl OnionMessageProvider for OnionHandler {
                fn next_onion_message_for_peer(&self, peer_node_id: PublicKey) -> Option<OnionMessage>;
            }

            impl OnionMessageHandler for OnionHandler {
                fn handle_onion_message(&self, peer_node_id: &PublicKey, msg: &OnionMessage);
                fn peer_connected(&self, their_node_id: &PublicKey, init: &Init, inbound: bool) -> Result<(), ()>;
                fn peer_disconnected(&self, their_node_id: &PublicKey);
                fn provided_node_features(&self) -> NodeFeatures;
                fn provided_init_features(&self, their_node_id: &PublicKey) -> InitFeatures;
            }
    }

    mock! {
        PeerProducer{}

        #[async_trait]
        impl PeerEventProducer for PeerProducer{
            async fn receive(&mut self) -> Result<PeerEvent, Status>;
            async fn onion_support(&mut self, pubkey: &PublicKey) -> bool;
        }
    }

    #[tokio::test]
    async fn test_consume_messenger_events() {
        let (sender, receiver) = channel(4);

        let pk = pubkey();
        let mut mock = MockOnionHandler::new();

        // Peer connected: onion messaging supported.
        sender
            .send(MessengerEvents::PeerConnected(pk, true))
            .await
            .unwrap();

        mock.expect_peer_connected()
            .withf(|_: &PublicKey, init: &Init, _: &bool| init.features.supports_onion_messages())
            .return_once(|_, _, _| Ok(()));

        // Peer connected: onion messaging not supported.
        sender
            .send(MessengerEvents::PeerConnected(pk, false))
            .await
            .unwrap();

        mock.expect_peer_connected()
            .withf(|_: &PublicKey, init: &Init, _: &bool| !init.features.supports_onion_messages())
            .return_once(|_, _, _| Ok(()));

        // Cover peer disconnected events.
        sender
            .send(MessengerEvents::PeerDisconnected(pk))
            .await
            .unwrap();
        mock.expect_peer_disconnected().return_once(|_| ());

        // Finally, send a producer exit event to test exit.
        sender
            .send(MessengerEvents::ProducerExit(
                ConsumerError::PeerProducerExit,
            ))
            .await
            .unwrap();

        let consume_err = consume_messenger_events(mock, receiver)
            .await
            .expect_err("consume should error");
        matches!(consume_err, ConsumerError::PeerProducerExit);
    }

    #[tokio::test]
    async fn test_consumer_exit_onion_messenger_failure() {
        let (sender, receiver) = channel(1);

        let pk = pubkey();
        let mut mock = MockOnionHandler::new();

        // Send a peer connected event, but mock out an error on the handler's connected function.
        sender
            .send(MessengerEvents::PeerConnected(pk, true))
            .await
            .unwrap();
        mock.expect_peer_connected().return_once(|_, _, _| Err(()));

        let consume_err = consume_messenger_events(mock, receiver)
            .await
            .expect_err("consume should error");
        matches!(consume_err, ConsumerError::OnionMessengerFailure);
    }

    #[tokio::test]
    async fn test_consumer_clean_exit() {
        // Test the case where our receiving channel is closed and we exit without error. Dropping
        // the sender manually has the effect of closing the channel.
        let (sender_done, receiver_done) = channel(1);
        drop(sender_done);

        assert!(
            consume_messenger_events(MockOnionHandler::new(), receiver_done)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_produce_peer_events() {
        let (sender, mut receiver) = channel(4);
        let (_exit_sender, exit) = channel(1);

        let mut mock = MockPeerProducer::new();

        // Peer connects and we successfully lookup its support for onion messages.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey().to_string(),
                r#type: i32::from(PeerOnline),
            })
        });
        mock.expect_onion_support().times(1).returning(|_| true);

        // Peer connects with no onion support.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey().to_string(),
                r#type: i32::from(PeerOnline),
            })
        });
        mock.expect_onion_support().returning(|_| false);

        // Peer disconnects.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey().to_string(),
                r#type: i32::from(PeerOffline),
            })
        });

        mock.expect_receive()
            .times(1)
            .returning(|| Err(Status::unknown("mock stream err")));

        matches!(
            produce_peer_events(mock, sender, exit)
                .await
                .expect_err("producer should error"),
            ProducerError::StreamError(_)
        );

        matches!(
            receiver.recv().await.unwrap(),
            MessengerEvents::PeerConnected(_, true)
        );

        matches!(
            receiver.recv().await.unwrap(),
            MessengerEvents::PeerConnected(_, false)
        );

        matches!(
            receiver.recv().await.unwrap(),
            MessengerEvents::PeerDisconnected(_)
        );

        matches!(
            receiver.recv().await.unwrap(),
            MessengerEvents::ProducerExit(_)
        );
    }

    #[tokio::test]
    async fn test_produce_peer_events_exit() {
        let (sender, _receiver) = channel(1);
        let (exit_sender, exit) = channel(1);

        let mock = MockPeerProducer::new();
        drop(exit_sender);
        assert!(produce_peer_events(mock, sender, exit).await.is_ok());
    }
}
