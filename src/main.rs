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
use std::sync::mpsc::{channel, Receiver, SendError, Sender};
use tonic_lnd::{
    lnrpc::peer_event::EventType::PeerOffline, lnrpc::peer_event::EventType::PeerOnline,
    lnrpc::NodeInfo, lnrpc::NodeInfoRequest, lnrpc::PeerEvent, tonic::Response, tonic::Status,
    Client, ConnectError,
};

const ONION_MESSAGE_OPTIONAL: u32 = 39;

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
        .get_info(tonic_lnd::lnrpc::GetInfoRequest {})
        .await
        .expect("failed to get info");

    let pubkey = PublicKey::from_str(&info.into_inner().identity_pubkey).unwrap();
    info!("Starting lndk for node: {pubkey}");

    // On startup, we want to get a list of our currently online peers to notify the onion messenger that they are
    // connected. This sets up our "start state" for the messenger correctly.
    let current_peers = client
        .lightning()
        .list_peers(tonic_lnd::lnrpc::ListPeersRequest {
            latest_error: false,
        })
        .await
        .expect("list peers failed")
        .into_inner()
        .peers;

    let mut peer_support = HashMap::new();
    for peer in current_peers {
        let pubkey = PublicKey::from_str(&peer.pub_key).unwrap();
        let onion_support = match block_on(client.lightning().get_node_info(NodeInfoRequest {
            pub_key: pubkey.to_string(),
            ..Default::default()
        })) {
            Ok(peer) => match peer.into_inner().node {
                Some(node) => node.features.contains_key(&ONION_MESSAGE_OPTIONAL),
                None => {
                    warn!("Peer {pubkey} not found in graph on startup, assuming no onion support");
                    false
                }
            },
            Err(e) => {
                warn!("Could not lookup peer {pubkey} in graph: {e}, assuming no onion message support");
                false
            }
        };

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

    run_onion_messenger(peer_support, onion_messenger)
}

// Responsible for initializing the onion messenger provided with the correct start state and managing onion message
// event producers and consumers.
fn run_onion_messenger<ES: Deref, NS: Deref, L: Deref, CMH: Deref>(
    current_peers: HashMap<PublicKey, bool>,
    onion_messenger: OnionMessenger<ES, NS, L, CMH>,
) -> Result<(), ()>
where
    ES::Target: EntropySource,
    NS::Target: NodeSigner,
    L::Target: Logger,
    CMH::Target: CustomOnionMessageHandler + Sized,
{
    // Setup channels that we'll use to communicate onion messenger events.
    let (sender, receiver) = channel();
    for (peer, onion_support) in current_peers {
        sender
            .send(MessengerEvents::PeerConnected(peer, onion_support))
            .map_err(|_| ())?
    }

    consume_messenger_events(onion_messenger, receiver).map_err(|e| {
        error!("run onion messenger: {e}");
    })
}

#[derive(Debug)]
enum ProducerError {
    SendError(SendError<MessengerEvents>),
    StreamError(MessengerEvents),
}

impl Error for ProducerError {}

impl fmt::Display for ProducerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ProducerError::SendError(e) => write!(f, "error sending messenger event: {e}"),
            ProducerError::StreamError(s) => write!(f, "LND stream {s} exited"),
        }
    }
}

trait PeerEventProducer {
    fn receive(&mut self) -> Result<PeerEvent, Status>;
    fn onion_support(&mut self, pubkey: &PublicKey) -> Result<bool, Box<dyn Error>>;
}

struct PeerStream<F: FnMut(&PublicKey) -> Result<Response<NodeInfo>, Status>> {
    peer_subscription: tonic_lnd::tonic::Streaming<PeerEvent>,
    node_info: F,
}

impl<F: FnMut(&PublicKey) -> Result<Response<NodeInfo>, Status>> PeerEventProducer
    for PeerStream<F>
{
    fn receive(&mut self) -> Result<PeerEvent, Status> {
        match block_on(self.peer_subscription.message()) {
            Ok(event) => match event {
                Some(peer_event) => Ok(peer_event),
                None => Err(Status::unknown("no event provided")),
            },
            Err(e) => Err(Status::unknown(format!("streaming error: {e}"))),
        }
    }

    fn onion_support(&mut self, pubkey: &PublicKey) -> Result<bool, Box<dyn Error>> {
        let resp = (self.node_info)(pubkey)?.into_inner();
        match resp.node {
            Some(node) => Ok(node.features.contains_key(&ONION_MESSAGE_OPTIONAL)),
            // If we couldn't find the node announcement, just assume that the node does not support onion messaging.
            None => {
                warn!(
                    "node {:?} not found in graph, assuming no onion message support",
                    pubkey
                );
                Ok(false)
            }
        }
    }
}

fn produce_peer_events(
    mut source: impl PeerEventProducer,
    events: Sender<MessengerEvents>,
) -> Result<(), ProducerError> {
    loop {
        match source.receive() {
            Ok(peer_event) => match peer_event.r#type() {
                PeerOnline => {
                    let pubkey = PublicKey::from_str(&peer_event.pub_key).unwrap();
                    match source.onion_support(&pubkey) {
                        Ok(onion_support) => {
                            let event = MessengerEvents::PeerConnected(pubkey, onion_support);
                            match events.send(event) {
                                Ok(_) => debug!("peer events sent: {event}"),
                                Err(err) => return Err(ProducerError::SendError(err)),
                            };
                        }
                        // If we couldn't lookup the peer's onion message support, just log the error and ignore the
                        // connection event.
                        Err(e) => {
                            warn!("onion support lookup failed for {pubkey}: {e}, ignoring connection event");
                            continue;
                        }
                    };
                }
                PeerOffline => {
                    let event = MessengerEvents::PeerDisconnected(
                        PublicKey::from_str(&peer_event.pub_key).unwrap(),
                    );
                    match events.send(event) {
                        Ok(_) => debug!("peer events sent: {event}"),
                        Err(err) => return Err(ProducerError::SendError(err)),
                    };
                }
            },
            Err(s) => {
                info!("peer events receive failed: {s}");

                let event = MessengerEvents::ProducerExit(ConsumerError::PeerProducerExit);
                match events.send(event) {
                    Ok(_) => debug!("peer events sent: {event}"),
                    Err(err) => error!("peer events: send producer exit failed: {err}"),
                }

                return Err(ProducerError::StreamError(event));
            }
        }
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
fn consume_messenger_events(
    onion_messenger: impl OnionMessageHandler,
    events: Receiver<MessengerEvents>,
) -> Result<(), ConsumerError> {
    loop {
        match events.recv() {
            Ok(onion_event) => match onion_event {
                MessengerEvents::PeerConnected(pubkey, onion_support) => {
                    info!("Consume messenger events received: {onion_event}");

                    let init_features = if onion_support {
                        let onion_message_optional: u64 = 1 << ONION_MESSAGE_OPTIONAL;
                        InitFeatures::from_le_bytes(onion_message_optional.to_le_bytes().to_vec())
                    } else {
                        InitFeatures::empty()
                    };

                    if onion_messenger
                        .peer_connected(
                            &pubkey,
                            &Init {
                                features: init_features,
                                remote_network_address: None,
                            },
                            false,
                        )
                        .is_err()
                    {
                        return Err(ConsumerError::OnionMessengerFailure);
                    };
                }
                MessengerEvents::PeerDisconnected(pubkey) => {
                    info!("Consume messenger events received: {onion_event}");
                    onion_messenger.peer_disconnected(&pubkey)
                }
                MessengerEvents::ProducerExit(e) => {
                    info!("Consume messenger events received: {onion_event}");
                    return Err(e);
                }
            },
            // If our receiver exits, there are no more events to consume so we can exit cleanly.
            Err(e) => {
                info!("Consumer messenger events received: {e}");
                return Ok(());
            }
        };
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    use bitcoin::secp256k1::PublicKey;
    use hex;
    use lightning::ln::features::{InitFeatures, NodeFeatures};
    use lightning::ln::msgs::{OnionMessage, OnionMessageHandler};
    use lightning::util::events::OnionMessageProvider;
    use mockall::mock;
    use std::sync::mpsc::channel;

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

        impl PeerEventProducer for PeerProducer{
            fn receive(&mut self) -> Result<PeerEvent, Status>;
            fn onion_support(&mut self, pubkey: &PublicKey) -> Result<bool, Box<dyn Error>>;
        }
    }

    #[test]
    fn test_consume_messenger_events() {
        let (sender, receiver) = channel();

        let pk = pubkey();
        let mut mock = MockOnionHandler::new();

        // Peer connected: onion messaging supported.
        sender
            .send(MessengerEvents::PeerConnected(pk, true))
            .unwrap();

        mock.expect_peer_connected()
            .withf(|_: &PublicKey, init: &Init, _: &bool| init.features.supports_onion_messages())
            .return_once(|_, _, _| Ok(()));

        // Peer connected: onion messaging not supported.
        sender
            .send(MessengerEvents::PeerConnected(pk, false))
            .unwrap();

        mock.expect_peer_connected()
            .withf(|_: &PublicKey, init: &Init, _: &bool| !init.features.supports_onion_messages())
            .return_once(|_, _, _| Ok(()));

        // Cover peer disconnected events.
        sender.send(MessengerEvents::PeerDisconnected(pk)).unwrap();
        mock.expect_peer_disconnected().return_once(|_| ());

        // Finally, send a producer exit event to test exit.
        sender
            .send(MessengerEvents::ProducerExit(
                ConsumerError::PeerProducerExit,
            ))
            .unwrap();

        let consume_err =
            consume_messenger_events(mock, receiver).expect_err("consume should error");
        matches!(consume_err, ConsumerError::PeerProducerExit);
    }

    #[test]
    fn test_consumer_exit_onion_messenger_failure() {
        let (sender, receiver) = channel();

        let pk = pubkey();
        let mut mock = MockOnionHandler::new();

        // Send a peer connected event, but mock out an error on the handler's connected function.
        sender
            .send(MessengerEvents::PeerConnected(pk, true))
            .unwrap();
        mock.expect_peer_connected().return_once(|_, _, _| Err(()));

        let consume_err =
            consume_messenger_events(mock, receiver).expect_err("consume should error");
        matches!(consume_err, ConsumerError::OnionMessengerFailure);
    }

    #[test]
    fn test_consumer_clean_exit() {
        // Test the case where our receiving channel is closed and we exit without error. Dropping
        // the sender manually has the effect of closing the channel.
        let (sender_done, receiver_done) = channel();
        drop(sender_done);
        assert!(consume_messenger_events(MockOnionHandler::new(), receiver_done).is_ok());
    }

    #[test]
    fn test_produce_peer_events() {
        let (sender, receiver) = channel();

        let mut mock = MockPeerProducer::new();

        // Peer connects and we successfully lookup its support for onion messages.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey().to_string(),
                r#type: i32::from(PeerOnline),
            })
        });
        mock.expect_onion_support().times(1).returning(|_| Ok(true));

        // Peer connects and we fail to lookup onion message support - no event should be sent. We don't need the
        // exact error type here, so just use a producer exit for convenience.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey().to_string(),
                r#type: i32::from(PeerOnline),
            })
        });
        mock.expect_onion_support()
            .returning(|_| Err(Box::new(ConsumerError::PeerProducerExit)));

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
            produce_peer_events(mock, sender).expect_err("producer should error"),
            ProducerError::StreamError(_)
        );

        matches!(
            receiver.recv().unwrap(),
            MessengerEvents::PeerConnected(_, _)
        );

        matches!(
            receiver.recv().unwrap(),
            MessengerEvents::PeerDisconnected(_)
        );

        matches!(receiver.recv().unwrap(), MessengerEvents::ProducerExit(_));
    }
}
