use crate::clock::TokioClock;
use crate::grpc::Retryable;
use crate::lnd::{features_support_onion_messages, PeerConnector, ONION_MESSAGES_OPTIONAL};
use crate::offers::connect_to_peer;
use crate::rate_limit::{RateLimiter, RateLimiterCfg, TokenLimiter};
use crate::{LifecycleSignals, LndkOnionMessenger, LDK_LOGGER_NAME};
use async_trait::async_trait;
use bitcoin::constants::ChainHash;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use core::ops::Deref;
use futures::executor::block_on;
use lightning::blinded_path::NodeIdLookUp;
use lightning::events::{Event, EventHandler, EventsProvider, ReplayEvent};
use lightning::ln::msgs::{Init, OnionMessage, OnionMessageHandler};
use lightning::onion_message::async_payments::AsyncPaymentsMessageHandler;
use lightning::onion_message::dns_resolution::DNSResolverMessageHandler;
use lightning::onion_message::messenger::{
    CustomOnionMessageHandler, MessageRouter, OnionMessenger,
};
use lightning::onion_message::offers::OffersMessageHandler;
use lightning::sign::NodeSigner;
use lightning::sign::{EntropySource, RandomBytes};
use lightning::types::features::InitFeatures;
use lightning::util::logger::{Level, Logger, Record};
use lightning::util::ser::{Readable, Writeable};
use log::{debug, error, info, trace, warn};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::io::Cursor;
use std::marker::Copy;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Notify;
use tokio::time::{sleep, timeout, Duration, Interval};
use tokio::{select, time};
use tonic_lnd::lnrpc::{ChanInfoRequest, ListPeersRequest, ListPeersResponse};
use tonic_lnd::tonic::Response;
use tonic_lnd::Client;
use tonic_lnd::{
    lnrpc::peer_event::EventType::PeerOffline, lnrpc::peer_event::EventType::PeerOnline,
    lnrpc::CustomMessage, lnrpc::PeerEvent, lnrpc::SendCustomMessageRequest,
    lnrpc::SendCustomMessageResponse, tonic::Status, LightningClient,
};
use triggered::Listener;

/// ONION_MESSAGE_TYPE is the message type number used in BOLT1 message types for onion messages.
const ONION_MESSAGE_TYPE: u32 = 513;

/// MSG_POLL_INTERVAL is the interval at which we poll for outgoing onion messages.
const MSG_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Node Id LookUp is a utility struct implementing NodeIdLookUp trait for LDK's OnionMessenger.
pub struct LndkNodeIdLookUp {
    client: Client,
    our_node_id: PublicKey,
}

impl LndkNodeIdLookUp {
    pub fn new(client: Client, our_node_id: PublicKey) -> Self {
        LndkNodeIdLookUp {
            client,
            our_node_id,
        }
    }
}

impl NodeIdLookUp for LndkNodeIdLookUp {
    fn next_node_id(&self, short_channel_id: u64) -> Option<PublicKey> {
        let get_chan_info_request = ChanInfoRequest {
            chan_id: short_channel_id,
            chan_point: "".to_string(),
        };
        let client = self.client.clone();
        match block_on(
            client
                .lightning_read_only()
                .get_chan_info(get_chan_info_request),
        ) {
            Ok(channel_info) => {
                let channel_info = channel_info.into_inner();
                let pubkey = if channel_info.node1_pub == self.our_node_id.to_string() {
                    channel_info.node2_pub
                } else {
                    channel_info.node1_pub
                };
                PublicKey::from_slice(pubkey.as_bytes()).ok()
            }
            Err(e) => {
                error!("Error getting channel info: {e}.");
                None
            }
        }
    }
}

/// MessengerUtilities is a utility struct used to provide Logger and EntropySource trait
/// implementations for LDK's OnionMessenger.
pub struct MessengerUtilities {
    entropy_source: RandomBytes,
}

impl MessengerUtilities {
    pub fn new(seed: [u8; 32]) -> Self {
        MessengerUtilities {
            entropy_source: RandomBytes::new(seed),
        }
    }
}

impl Default for MessengerUtilities {
    fn default() -> Self {
        let mut entropy_source = ChaCha20Rng::from_entropy();
        let mut chacha_bytes: [u8; 32] = [0; 32];
        entropy_source.fill_bytes(&mut chacha_bytes);
        Self::new(chacha_bytes)
    }
}

impl EntropySource for MessengerUtilities {
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        self.entropy_source.get_secure_random_bytes()
    }
}

impl Deref for MessengerUtilities {
    type Target = RandomBytes;

    fn deref(&self) -> &Self::Target {
        &self.entropy_source
    }
}

impl Logger for MessengerUtilities {
    fn log(&self, record: Record) {
        let args_str = record.args.to_string();
        match record.level {
            Level::Gossip => {}
            Level::Trace => trace!(target: LDK_LOGGER_NAME, "{}", args_str),
            Level::Debug => debug!(target: LDK_LOGGER_NAME, "{}", args_str),
            Level::Info => info!(target: LDK_LOGGER_NAME, "{}", args_str),
            Level::Warn => warn!(target: LDK_LOGGER_NAME, "{}", args_str),
            Level::Error => error!(target: LDK_LOGGER_NAME, "{}", args_str),
        }
    }
}

impl LndkOnionMessenger {
    /// run_onion_messenger is the main event loop for connecting an OnionMessenger to LND's various
    /// APIs to handle onion messages externally to LND. It follows a producer / consumer
    /// pattern, with many producers creating MessengerEvents that are handled by a single
    /// consumer that drives the OnionMessenger accordingly. This function will block until
    /// consumer errors or one of the producers exits.
    ///
    /// Producers:
    /// 1. Peer Events: Sourced from LND's PeerEventSubscription API, produces peer online and
    ///    offline events.
    /// 2. Incoming Messages: Sourced from LND's SubscribeCustomMessages API, produces incoming
    ///    onion message events.
    /// 3. Outgoing Poll: Using a simple ticker, produces polling events to check for outgoing onion
    ///    messages.
    ///
    /// The main consumer processes one MessengerEvent at a time, applying basic rate limiting to
    /// each peer to prevent spam.
    pub(crate) async fn run_onion_messenger<
        ES: Deref,
        NS: Deref,
        L: Deref,
        NL: Deref,
        MR: Deref,
        OMH: Deref,
        APH: Deref,
        DRH: Deref,
        CMH: Deref,
    >(
        &self,
        ln_client: &mut tonic_lnd::LightningClient,
        onion_messenger: OnionMessenger<ES, NS, L, NL, MR, OMH, APH, DRH, CMH>,
        network: Network,
        signals: LifecycleSignals,
        rate_limiter_cfg: RateLimiterCfg,
    ) -> Result<(), ()>
    where
        ES::Target: EntropySource,
        NS::Target: NodeSigner,
        L::Target: Logger,
        NL::Target: NodeIdLookUp,
        MR::Target: MessageRouter,
        OMH::Target: OffersMessageHandler,
        APH::Target: AsyncPaymentsMessageHandler,
        DRH::Target: DNSResolverMessageHandler,
        CMH::Target: CustomOnionMessageHandler + Sized,
    {
        // On startup, we want to get a list of our currently online peers to notify the onion
        // messenger that they are connected. This sets up our "start state" for the
        // messenger correctly.
        let current_peers = get_current_peers(&mut ln_client.clone()).await?;

        // Setup channels that we'll use to communicate onion messenger events. We buffer our
        // channels by the number of peers (+1 because we require a non-zero buffer) that
        // the node currently has so that we can send all of our startup online events in
        // one go (before we boot up the consumer). The number of peers that we have is also
        // related to the number of events we can expect to process, so it's a sensible enough
        // buffer size.
        let (sender, receiver) = channel(current_peers.len() + 1);
        for (peer, onion_support) in current_peers.clone() {
            sender
                .send(MessengerEvents::PeerConnected(peer, onion_support))
                .await
                .map_err(|e| {
                    error!("Notify peer connected: {e}.");
                })?
        }

        let mut set = tokio::task::JoinSet::new();

        // Setup channels that we'll use to communicate whenever a restart event happens.
        // Whenever a new event is consumed we will get current peers and message peers
        // where reconnected.
        let reconnected_notification = Arc::new(Notify::new());
        let reconnected_notification_custom_messages = reconnected_notification.clone();
        let (reconnection_shutdown, reconnection_listener) =
            (signals.shutdown.clone(), signals.listener.clone());
        let reconnection_notification_receiver = reconnected_notification.clone();
        let mut reconnection_client = Retryable::new(ln_client.clone());
        let reconnection_sender = sender.clone();
        set.spawn(async move {
            match receive_reconnection_event(
                &mut reconnection_client,
                reconnection_notification_receiver,
                reconnection_sender,
                reconnection_listener,
            )
            .await
            {
                Ok(_) => debug!("Reconnection listener completed"),
                Err(e) => {
                    error!("Notify peer connected: {e}");
                    reconnection_shutdown.trigger();
                }
            }
        });

        // Subscribe to peer events from LND first thing so that we don't miss any online/offline
        // events while we are starting up. The onion messenger can handle superfluous
        // online/offline reports, so it's okay if this ends up creating some duplicate
        // events. The event subscription from LND blocks until it gets its first event (which
        // could take very long), so we get the subscription itself inside of our producer thread.
        let mut peers_client = Retryable::new(ln_client.clone());
        let peers_sender = sender.clone();
        let (peers_shutdown, peers_listener) = (signals.shutdown.clone(), signals.listener.clone());
        set.spawn(async move {
            let peer_subscription = match peers_client
                .with_infinite_retries(
                    LightningClient::subscribe_peer_events,
                    tonic_lnd::lnrpc::PeerEventSubscription {},
                    Some(reconnected_notification),
                )
                .await
            {
                Ok(response) => {
                    info!("Connected to peer events subscription.");
                    response.into_inner()
                }
                Err(e) => {
                    peers_shutdown.trigger();
                    error!("Error subscribing to peer events: {e}.");
                    return;
                }
            };

            let peer_stream = PeerStream {
                peer_subscription,
                client: peers_client.into_inner(),
            };

            match produce_peer_events(peer_stream, peers_sender, peers_listener).await {
                Ok(_) => debug!("Peer events producer exited."),
                Err(e) => {
                    peers_shutdown.trigger();
                    error!("Peer events producer exited: {e}.");
                }
            };
        });

        // Setup channels that we'll use to communicate whenever a restart event happens.
        // Whenever a new event is consumed we will try to subscribe again to custom messages
        let mut messages_client = Retryable::new(ln_client.clone());
        let in_msg_sender = sender.clone();
        let (reconnection_shutdown, messages_listener) =
            (signals.shutdown.clone(), signals.listener.clone());
        let reconnected_notification_clone_receiver =
            reconnected_notification_custom_messages.clone();
        set.spawn(async move {
            match receive_reconnection_subscription_event(
                &mut messages_client,
                messages_listener,
                reconnected_notification_clone_receiver,
                in_msg_sender,
            )
            .await
            {
                Ok(_) => debug!("Reconnection listener completed"),
                Err(e) => {
                    error!("Notify peer connected: {e}");
                    reconnection_shutdown.trigger();
                }
            }
        });

        // Subscribe to custom messaging events from LND so that we can receive incoming messages.
        let mut messages_client = Retryable::new(ln_client.clone());
        let in_msg_sender = sender.clone();
        let (messages_shutdown, messages_listener) =
            (signals.shutdown.clone(), signals.listener.clone());
        set.spawn(async move {
            let message_subscription = match messages_client
                .with_infinite_retries(
                    LightningClient::subscribe_custom_messages,
                    tonic_lnd::lnrpc::SubscribeCustomMessagesRequest {},
                    None,
                )
                .await
            {
                Ok(response) => {
                    info!("Connected to message subscription.");
                    response.into_inner()
                }
                Err(e) => {
                    messages_shutdown.trigger();
                    error!("Error subscribing to message events: {e}.");
                    return;
                }
            };

            let message_stream = MessageStream {
                message_subscription,
            };

            match produce_incoming_message_events(
                message_stream,
                in_msg_sender.clone(),
                messages_listener.clone(),
            )
            .await
            {
                Ok(_) => debug!("Message events producer exited."),
                Err(e) => {
                    reconnected_notification_custom_messages.notify_waiters();
                    error!("Message events producer exited: {e}.");
                }
            }
        });

        // Spin up a ticker that polls at an interval for any outgoing messages so that we can pass
        // on outgoing messages to LND.
        let mut interval = time::interval(MSG_POLL_INTERVAL);
        // Set the missed tick behaviour to skip as we don't want to fill the queue with
        // `MessengerEvents::SendOutgoing` events in case of a delay.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (events_shutdown, events_listener) =
            (signals.shutdown.clone(), signals.listener.clone());
        set.spawn(async move {
            match produce_outgoing_message_events(sender, events_listener, interval).await {
                Ok(_) => debug!("Outgoing message events producer exited."),
                Err(e) => {
                    events_shutdown.trigger();
                    error!("Outgoing message events producer exited: {e}.");
                }
            }
        });

        // Consume events is our main controlling loop, so we run it inline here. We use a RefCell
        // in onion_messenger to allow interior mutability (see LndNodeSigner) so this
        // function can't safely be passed off to another thread. This function is expected
        // to finish if any producing thread exits (because we're no longer receiving the
        // events we need).
        let rate_limiter = &mut TokenLimiter::new(
            current_peers.keys().copied(),
            rate_limiter_cfg.call_count,
            rate_limiter_cfg.call_period_secs,
            TokioClock::new(),
        );
        let mut message_sender = CustomMessenger {
            client: Retryable::new(ln_client.clone()),
        };
        let event_handler = LndkEventHandler {
            lnd_client: ln_client.clone(),
        };
        let consume_result = consume_messenger_events(
            onion_messenger,
            receiver,
            &mut message_sender,
            rate_limiter,
            event_handler,
            network,
        )
        .await;
        match consume_result {
            Ok(_) => info!("Consume messenger events exited."),
            Err(e) => {
                error!("Consume messenger events exited: {e}.");
            }
        }

        // Tasks will independently exit, so we can assert that they do so in any order.
        let mut task_err = false;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(_) => info!("Producer exited."),
                Err(_) => {
                    task_err = true;
                    error!("Producer exited with an error.");
                }
            };
        }
        // Exit with an error if any task did not exit cleanly.
        if consume_result.is_err() || task_err {
            return Err(());
        }

        Ok(())
    }
}

async fn get_current_peers(
    client: &mut tonic_lnd::LightningClient,
) -> Result<HashMap<PublicKey, bool>, ()> {
    debug!("Getting current peers");
    let current_peers = client
        .list_peers(tonic_lnd::lnrpc::ListPeersRequest {
            latest_error: false,
        })
        .await
        .map_err(|e| {
            error!("Could not lookup current peers: {e}");
        })?;
    let peer_support = build_peer_support_from_response(current_peers);
    Ok(peer_support)
}

fn build_peer_support_from_response(
    current_peers: Response<ListPeersResponse>,
) -> HashMap<PublicKey, bool> {
    let mut peer_support = HashMap::new();
    for peer in current_peers.into_inner().peers {
        let pubkey = PublicKey::from_str(&peer.pub_key).expect("Public Key not valid");
        let onion_support = features_support_onion_messages(&peer.features);
        peer_support.insert(pubkey, onion_support);
    }
    peer_support
}

/// lookup_onion_support performs a best-effort lookup in the node's list of current peers to
/// determine whether it supports onion messaging. If the node is not found a warning is logged and
/// we assume that onion messaging is not supported.
async fn lookup_onion_support(pubkey: &PublicKey, client: &mut tonic_lnd::LightningClient) -> bool {
    match client
        .list_peers(tonic_lnd::lnrpc::ListPeersRequest {
            latest_error: false,
        })
        .await
    {
        Ok(peers) => {
            for peer in peers.into_inner().peers {
                if peer.pub_key != pubkey.to_string() {
                    continue;
                }

                // Sometimes if the connection to the peer is very new, we have to wait for the
                // features map to populate as non-empty.
                let check_empty_timeout = 5;
                let features = match timeout(
                    Duration::from_secs(check_empty_timeout),
                    check_empty_features(pubkey, client.clone()),
                )
                .await
                {
                    Ok(features) => features,
                    Err(_) => {
                        warn!(
                            "Did not get non-empty feature set from peer {} set in {} seconds.",
                            peer.pub_key, check_empty_timeout
                        );
                        peer.features
                    }
                };

                return features_support_onion_messages(&features);
            }

            warn!("Peer {pubkey} not found in current set of peers, assuming no onion support.");
            false
        }
        Err(e) => {
            warn!("Could not lookup peers for {pubkey}: {e}, assuming no onion message support.");
            false
        }
    }
}

// check_empty_features repeatedly looks up the peer's feature set until it returns a non-empty
// map. Sometimes if a peer is new, LND needs a little time to update the feature set.
async fn check_empty_features(
    pubkey: &PublicKey,
    mut client: LightningClient,
) -> HashMap<u32, tonic_lnd::lnrpc::Feature> {
    loop {
        match client
            .list_peers(tonic_lnd::lnrpc::ListPeersRequest {
                latest_error: false,
            })
            .await
        {
            Ok(peers) => {
                for peer in peers.into_inner().peers {
                    if peer.pub_key != pubkey.to_string() {
                        continue;
                    }

                    if !peer.features.is_empty() {
                        return peer.features;
                    }
                }
            }
            Err(_) => {
                warn!("error connecting to listpeers");
                continue;
            }
        };
        sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Debug)]
/// ProducerError represents the exit of a producing loop.
enum ProducerError {
    /// SendError indicates that a producer could not send a messenger event, likely due to
    /// consumer shutdown.
    SendError(String),
    /// StreamError indicates that LND's stream has terminated, either due to error or shutdown of
    /// the underlying node.
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
/// PeerEventProducer provides a layer of abstraction over LND's peer events subscription.
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
            _none => Err(Status::unknown("no event provided")),
        }
    }

    async fn onion_support(&mut self, pubkey: &PublicKey) -> bool {
        lookup_onion_support(pubkey, &mut self.client).await
    }
}

#[async_trait]
trait ReconnectionClient: Send + Sync {
    async fn list_peers_with_retries(
        &mut self,
        request: ListPeersRequest,
    ) -> Result<Response<ListPeersResponse>, Status>;
}

#[async_trait]
impl ReconnectionClient for Retryable<LightningClient> {
    async fn list_peers_with_retries(
        &mut self,
        request: ListPeersRequest,
    ) -> Result<Response<ListPeersResponse>, Status> {
        self.with_infinite_retries(LightningClient::list_peers, request, None)
            .await
    }
}

async fn receive_reconnection_event(
    client: &mut impl ReconnectionClient,
    reconnection_listener: Arc<Notify>,
    sender: Sender<MessengerEvents>,
    shutdown_listener: Listener,
) -> Result<(), Box<dyn Error>> {
    loop {
        select! {
            biased;
            _ = shutdown_listener.clone() => {
                info!("Received shutdown signal, exiting reconnection listener.");
                return Ok(())
            }

            _ = reconnection_listener.notified() => {
                info!("Received reconnection event.");
                let request = tonic_lnd::lnrpc::ListPeersRequest {
                    latest_error: false
                };

                let list_peer_response = client.list_peers_with_retries(request).await?;
                debug!("Successfully fetched peers from reconnection to LND.");
                let current_peers = build_peer_support_from_response(list_peer_response);
                for (peer, onion_support) in current_peers.clone() {
                    sender
                    .send(MessengerEvents::PeerConnected(peer, onion_support))
                    .await?;
                }
                debug!("Reconnection listener completed.");

            }

        }
    }
}

async fn receive_reconnection_subscription_event(
    messages_client: &mut Retryable<LightningClient>,
    shutdown_listener: Listener,
    reconnection_listener: Arc<Notify>,
    in_msg_sender: Sender<MessengerEvents>,
) -> Result<(), Box<dyn Error>> {
    loop {
        select! {
            biased;
            _ = shutdown_listener.clone() => {
                info!("Received shutdown signal, exiting reconnection subscription listener.");
                return Ok(())
            }
            _ = reconnection_listener.notified() => {
                    let message_subscription = match messages_client
                        .with_infinite_retries(
                            LightningClient::subscribe_custom_messages,
                            tonic_lnd::lnrpc::SubscribeCustomMessagesRequest {},
                            None,
                        )
                        .await
                    {
                        Ok(response) => {
                            info!("Connected to message subscription.");
                            response.into_inner()
                        }
                        Err(e) => {
                            error!("Error subscribing to message events: {e}.");
                            return Ok(())
                        }
                    };

                    let message_stream = MessageStream {
                        message_subscription,
                    };
                    match produce_incoming_message_events(message_stream, in_msg_sender.clone(), shutdown_listener.clone())
                        .await {
                            Ok(_) => debug!("Message events producer exited."),
                            Err(e) => {
                                error!("Message events producer exited: {e}.");
                            }
                        }
            }
        }
    }
}

/// Consumes a stream of peer online/offline events from the PeerEventProducer until the stream
/// exits (by sending an error) or the producer receives the signal to exit (via close of the exit
/// channel).
///
/// Note that this function *must* send an exit error to the Sender provided on all exit-cases, so
/// that upstream consumers know to exit as well. Failures related to sending events are an
/// exception, as failure to send indicates that the consumer has already exited (the receiving end
/// of the channel has hung up), and we can't send any more events anyway.
async fn produce_peer_events(
    mut source: impl PeerEventProducer,
    events: Sender<MessengerEvents>,
    listener: Listener,
) -> Result<(), ProducerError> {
    loop {
        select! (
            // Select biased so that we'll always check our exit signal before attempting to receive. This allows more
            // deterministic tests, and ensures that the producer will exit when requested (and won't queue up a series
            // of events that can't be consumed, possibly blocking if the channel buffer is small).
            biased;

            _ = listener.clone() => {
                info!("Peer events received signal to quit.");
                return Ok(())
            }
            peer_event = source.receive() => {
                match peer_event {
                    Ok(peer_event) => match peer_event.r#type() {
                        PeerOnline => {
                            let pubkey = PublicKey::from_str(&peer_event.pub_key).unwrap();
                            let onion_support = source.onion_support(&pubkey).await;
                            let event = MessengerEvents::PeerConnected(pubkey, onion_support);
                            let event_str = format!("{event:?}");
                            match events.send(event).await {
                                Ok(_) => debug!("Peer events sent: {event_str}."),
                                Err(err) => return Err(ProducerError::SendError(format!("{err}"))),
                            };
                        }
                        PeerOffline => {
                            let event = MessengerEvents::PeerDisconnected(PublicKey::from_str(&peer_event.pub_key).unwrap());
                            let event_str = format!("{event:?}");
                            match events.send(event).await {
                                Ok(_) => debug!("Peer events sent: {event_str}."),
                                Err(err) => return Err(ProducerError::SendError(format!("{err}"))),
                            };
                        }
                    },
                    Err(s) => {
                        info!("Peer events receive failed: {s}.");

                        let event = MessengerEvents::ProducerExit(ConsumerError::PeerProducerExit);
                        let event_str = format!("{event:?}");
                        match events.send(event).await {
                            Ok(_) => debug!("Peer events sent: {event_str}."),
                            Err(err) => error!("Peer events: send producer exit failed: {err}."),
                        }
                        return Err(ProducerError::StreamError(format!("{s}")));
                    }
                };
            }
        );
    }
}

#[async_trait]
/// IncomingMessageProducer prodices a layer of abstraction over LND's custom messaging subscription
/// for incoming messages.
trait IncomingMessageProducer {
    async fn receive(&mut self) -> Result<CustomMessage, Status>;
}

struct MessageStream {
    message_subscription: tonic_lnd::tonic::Streaming<CustomMessage>,
}

#[async_trait]
impl IncomingMessageProducer for MessageStream {
    async fn receive(&mut self) -> Result<CustomMessage, Status> {
        match self.message_subscription.message().await? {
            Some(msg) => Ok(msg),
            None => Err(Status::unknown("no message provided")),
        }
    }
}

/// Consumes a stream of incoming message events from the IncomingMessageProducer until the stream
/// exits (by sending an error) or the producer receives the signal to exit (via close of the exit
/// channel).
///
/// Note that this function *must* send an exit error to the Sender provided on all exit-cases, so
/// that upstream consumers know to exit as well. Failures related to sending events are an
/// exception, as failure to send indicates that the consumer has already exited (the receiving end
/// of the channel has hung up), and we can't send any more events anyway.
async fn produce_incoming_message_events(
    mut source: impl IncomingMessageProducer,
    events: Sender<MessengerEvents>,
    listener: Listener,
) -> Result<(), ProducerError> {
    loop {
        select! (
        // Select biased so that we'll always check our exit signal before attempting to receive. This allows more
        // deterministic tests, and ensures that the producer will exit when requested (and won't queue up a series
        // of events that can't be consumed, possibly blocking if the channel buffer is small).
        biased;

        _ = listener.clone() => {
            info!("Peer events received signal to quit.");
            return Ok(())
        }
        onion_message = source.receive() => {
            match onion_message {
                Ok(incoming_message) => {
                    if incoming_message.r#type != ONION_MESSAGE_TYPE {
                        trace!("Ignoring custom message: {}.", incoming_message.r#type);
                        continue;
                    }

                    let pubkey = PublicKey::from_slice(&incoming_message.peer).unwrap();
                    let res = OnionMessage::read(&mut Cursor::new(incoming_message.data));
                    match res {
                        Ok(onion_message) => {
                            let event = MessengerEvents::IncomingMessage(pubkey, onion_message);
                            let event_str = format!("{event:?}");
                            match events.send(event).await {
                                Ok(_) => debug!("Incoming messages sent: {event_str}."),
                                Err(err) => return Err(ProducerError::SendError(format!("{err}"))),
                            };
                        },
                        Err(e) => error!("Invalid onion message from: {pubkey}: {e}."),
                    };
                },
                Err(s) => {
                    info!("Incoming message events receive failed: {s}.");
                    let event = MessengerEvents::ProducerExit(ConsumerError::IncomingMessageProducerExit);
                    let event_str = format!("{event:?}");
                    match events.send(event).await {
                        Ok(_) => debug!("Incoming message events sent: {event_str}."),
                        Err(err) => error!("Incoming message events: send producer exit failed: {err}."),
                    };

                    return Err(ProducerError::StreamError(format!("{s}")));
                },
            };
        });
    }
}

#[derive(Debug, Copy, Clone)]
/// ConsumerError represents exit from the main consumer loop.
enum ConsumerError {
    /// Internal onion messenger implementation has experienced an error.
    OnionMessengerFailure,

    /// The producer responsible for peer connection events has exited.
    PeerProducerExit,

    /// The producer responsible for incoming messages has exited.
    IncomingMessageProducerExit,
}

impl Error for ConsumerError {}

impl fmt::Display for ConsumerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConsumerError::OnionMessengerFailure => {
                write!(f, "consumer err: onion messenger failure")
            }
            ConsumerError::PeerProducerExit => write!(f, "consumer err: peer producer exit"),
            ConsumerError::IncomingMessageProducerExit => {
                write!(f, "consumer err: incoming message producer exit")
            }
        }
    }
}

/// MessengerEvents represents all of the events that are relevant to onion messages.
#[derive(Debug, Clone)]
enum MessengerEvents {
    PeerConnected(PublicKey, bool),
    PeerDisconnected(PublicKey),
    IncomingMessage(PublicKey, OnionMessage),
    SendOutgoing,
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
            MessengerEvents::IncomingMessage(p, _) => {
                write!(f, "messenger event: {p} incoming onion message")
            }
            MessengerEvents::SendOutgoing => {
                write!(f, "messenger event: poll for new outgoing onion messages")
            }
            MessengerEvents::ProducerExit(s) => write!(f, "messenger event: {s} exited"),
        }
    }
}

/// consume_messenger_events receives a series of onion messaging related events and delivers them
/// to the OnionMessenger provided, using the RateLimiter to limit resources consumed by each peer.
async fn consume_messenger_events(
    onion_messenger: impl OnionMessageHandler + EventsProvider,
    mut events: Receiver<MessengerEvents>,
    message_sender: &mut impl SendCustomMessage,
    rate_limiter: &mut impl RateLimiter,
    event_handler: impl EventHandler,
    network: Network,
) -> Result<(), ConsumerError> {
    let chain_hash = ChainHash::using_genesis_block_const(network);
    let network = vec![chain_hash];

    while let Some(onion_event) = events.recv().await {
        match onion_event {
            // We don't want to log SendOutgoing events, since we send out this event every 100 ms.
            MessengerEvents::SendOutgoing => {}
            _ => info!("Consume messenger events received: {onion_event}."),
        };

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
                        pubkey,
                        &Init {
                            features: init_features,
                            remote_network_address: None,
                            networks: Some(network.clone()),
                        },
                        false,
                    )
                    .map_err(|_| ConsumerError::OnionMessengerFailure)?;

                // In addition to keeping the onion messenger up to date with the latest peers, we
                // need to keep our local version up to date so we send outgoing OMs
                // all of our peers.
                rate_limiter.peer_connected(pubkey);
            }
            MessengerEvents::PeerDisconnected(pubkey) => {
                onion_messenger.peer_disconnected(pubkey);

                // In addition to keeping the onion messenger up to date with the latest peers, we
                // need to keep our local version up to date so we send outgoing OMs
                // to our correct peers.
                rate_limiter.peer_disconnected(pubkey);
            }
            MessengerEvents::IncomingMessage(pubkey, onion_message) => {
                if !rate_limiter.query_peer(pubkey) {
                    info!("Peer: {pubkey} hit rate limit, dropping incoming onion message");
                    continue;
                }

                onion_messenger.handle_onion_message(pubkey, &onion_message)
            }
            MessengerEvents::SendOutgoing => {
                onion_messenger.process_pending_events(&event_handler);

                for peer in rate_limiter.peers() {
                    if let Some(msg) = onion_messenger.next_onion_message_for_peer(peer) {
                        info!("Sending outgoing onion message to {peer}.");
                        relay_outgoing_msg_event(&peer, msg, message_sender).await;
                    }
                }
            }
            MessengerEvents::ProducerExit(e) => {
                // Only logging about the ProducerExit event.
                // Keep the loop still running so that when the reconnection process is done
                // it will be possible to keep processing messenger events.
                error!("ProducerExit {e}");
                continue;
            }
        }
    }

    Ok(())
}

#[async_trait]
/// SendCustomMessage provides a level of abstraction over LND's send custom message API.
trait SendCustomMessage {
    async fn send_custom_message(
        &mut self,
        request: SendCustomMessageRequest,
    ) -> Result<SendCustomMessageResponse, Status>;
}

struct CustomMessenger {
    client: Retryable<LightningClient>,
}

#[async_trait]
impl SendCustomMessage for CustomMessenger {
    async fn send_custom_message(
        &mut self,
        request: SendCustomMessageRequest,
    ) -> Result<SendCustomMessageResponse, Status> {
        match self
            .client
            .with_max_attempts(LightningClient::send_custom_message, request, Some(3))
            .await
        {
            Ok(resp) => Ok(resp.into_inner()),
            Err(status) => Err(status),
        }
    }
}

/// produce_outgoing_message_events is produce for producing outgoing message events at a regular
/// interval.
///
/// Note that this function *must* send an exit error to the Sender provided on all exit-cases, so
/// that upstream consumers know to exit as well. Failures related to sending events are an
/// exception, as failure to send indicates that the consumer has already exited (the receiving end
/// of the channel has hung up), and we can't send any more events anyway.
async fn produce_outgoing_message_events(
    events: Sender<MessengerEvents>,
    listener: Listener,
    mut interval: Interval,
) -> Result<(), ProducerError> {
    loop {
        select! (
            // Select biased so that we'll always check our exit signal before attempting to receive. This allows more
            // deterministic tests, and ensures that the producer will exit when requested (and won't queue up a series
            // of events that can't be consumed, possibly blocking if the channel buffer is small).
            biased;

            _ = listener.clone() => {
                info!("Outgoing messenger events received signal to quit.");
                return Ok(());
            }

            _ = interval.tick() => {
                events.send(MessengerEvents::SendOutgoing).await
                    .map_err(|e| {
                        ProducerError::SendError(format!("{e}"))
                    })?;
            }
        )
    }
}

/// relay_outgoing_msg_event is responsible for passing along new outgoing messages from peers. If a
/// new onion message turns up, it will pass it along to lnd.
async fn relay_outgoing_msg_event(
    peer: &PublicKey,
    msg: OnionMessage,
    ln_client: &mut impl SendCustomMessage,
) {
    let mut buf = vec![];
    match msg.write(&mut buf) {
        Ok(_) => {}
        Err(err) => {
            error!("Error writing onion message: {}.", err);
            return;
        }
    }

    // Relay this message to LND.
    let req = tonic_lnd::lnrpc::SendCustomMessageRequest {
        peer: peer.serialize().to_vec(),
        r#type: ONION_MESSAGE_TYPE,
        data: buf,
    };

    // TODO: To improve resilience, retry this call in the event of a temporary connection error.
    match ln_client.send_custom_message(req).await {
        Ok(_) => debug!("Sent outgoing onion message {msg:?} to {peer}."),
        Err(e) => error!("Error sending custom message {e} to {peer}."),
    }
}

struct LndkEventHandler<T: PeerConnector + Send + 'static + Clone> {
    lnd_client: T,
}

impl<T: PeerConnector + Send + 'static + Clone> EventHandler for LndkEventHandler<T> {
    fn handle_event(&self, event: Event) -> Result<(), ReplayEvent> {
        match event {
            Event::ConnectionNeeded {
                node_id,
                addresses: _,
            } => {
                debug!("ConnectionNeeded event received for node: {}", node_id);
                let lnd_client = self.lnd_client.clone();

                // TODO: we probably want to retry this connect_to_peer call if it fails
                tokio::spawn(async move {
                    if let Err(e) = connect_to_peer(lnd_client, node_id).await {
                        error!("Failed to connect to peer: {}", e);
                    }
                });
                Ok(())
            }
            Event::OnionMessageIntercepted { .. } => Ok(()),
            Event::OnionMessagePeerConnected { .. } => Ok(()),
            _ => Ok(()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_utils::pubkey;
    use bitcoin::secp256k1::PublicKey;
    use bitcoin::Network;
    use std::sync::Arc;

    use bytes::BufMut;
    use lightning::events::{EventHandler, EventsProvider};
    use lightning::ln::msgs::{OnionMessage, OnionMessageHandler};
    use lightning::util::ser::Readable;
    use lightning::util::ser::Writeable;
    use mockall::mock;
    use std::io::Cursor;
    use tokio::sync::mpsc::channel;

    /// Produces an OnionMessage that can be used for tests. We need to manually write individual
    /// bytes because onion messages in LDK can only be created using read/write impls that deal
    /// with raw bytes (since some other fields are not public).
    fn onion_message() -> OnionMessage {
        let mut w = vec![];
        let pubkey_bytes = pubkey(0).serialize();

        // Blinding point for the onion message.
        w.put_slice(&pubkey_bytes);

        // Write the length of the onion packet:
        // Version: 1
        // Ephemeral Key: 33
        // Hop Payloads: 1300
        // HMAC: 32.
        w.put_u16(1 + 33 + 1300 + 32);

        // Write meaningless contents for the actual values.
        w.put_u8(0);
        w.put_slice(&pubkey_bytes);
        w.put_bytes(1, 1300);
        w.put_bytes(2, 32);

        let mut readable = Cursor::new(w);
        OnionMessage::read(&mut readable).unwrap()
    }

    mock! {
            OnionHandler{}

            impl OnionMessageHandler for OnionHandler {
                fn handle_onion_message(&self, peer_node_id: PublicKey, msg: &OnionMessage);
                fn next_onion_message_for_peer(&self, peer_node_id: PublicKey) -> Option<OnionMessage>;
                fn peer_connected(&self, their_node_id: PublicKey, init: &Init, inbound: bool) -> Result<(), ()>;
                fn peer_disconnected(&self, their_node_id: PublicKey);
                fn timer_tick_occurred(&self);
                fn provided_node_features(&self) -> lightning::types::features::NodeFeatures;
                fn provided_init_features(&self, their_node_id: PublicKey) -> lightning::types::features::InitFeatures;
            }
    }

    mock! {
        TestPeerConnector{}

        impl Clone for TestPeerConnector {
            fn clone(&self) -> Self;
        }

         #[async_trait]
         impl PeerConnector for TestPeerConnector {
             async fn list_peers(&mut self) -> Result<tonic_lnd::lnrpc::ListPeersResponse, Status>;
             async fn get_node_info(&mut self, pub_key: String, include_channels: bool) -> Result<tonic_lnd::lnrpc::NodeInfo, Status>;
             async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
         }
    }

    // Mockall can't automatically produce a mock for EventsProvider since mockall requires generic
    // parameters be 'static. See: https://docs.rs/mockall/latest/mockall/#generic-traits-and-structs. So we add the trait to our
    // mock manually here.
    impl EventsProvider for MockOnionHandler {
        fn process_pending_events<H: Deref>(&self, _handler: H)
        where
            H::Target: EventHandler,
        {
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

    mock! {
        SendCustomMessenger{}

        #[async_trait]
         impl SendCustomMessage for SendCustomMessenger{
             async fn send_custom_message(&mut self, request: SendCustomMessageRequest) -> Result<SendCustomMessageResponse, Status>;
         }
    }

    mock! {
        RateLimiter{}

        impl RateLimiter for RateLimiter{
            fn peer_connected(&mut self, peer_key: PublicKey);
            fn peer_disconnected(&mut self, peer_key: PublicKey);
            fn peers(&self) -> Vec<PublicKey>;
            fn query_peer(&mut self, peer_key: PublicKey) -> bool;
        }
    }
    mock! {
        ReconnectionClient {}

        #[async_trait]
        impl ReconnectionClient for ReconnectionClient {
            async fn list_peers_with_retries(
                &mut self,
                request: ListPeersRequest,
            ) -> Result<Response<ListPeersResponse>, Status>;
        }
    }

    #[tokio::test]
    async fn test_receive_reconection_event() {
        let (sender, mut receiver) = channel(4);
        let (_shutdown, listener) = triggered::trigger();
        let reconnect_notification = Arc::new(Notify::new());

        let mut mock_client = MockReconnectionClient::new();

        let pubkey1 = pubkey(1);
        let pubkey2 = pubkey(2);
        let mock_peers = vec![
            tonic_lnd::lnrpc::Peer {
                pub_key: pubkey1.to_string(),
                features: HashMap::new(),
                ..Default::default()
            },
            tonic_lnd::lnrpc::Peer {
                pub_key: pubkey2.to_string(),
                features: {
                    let mut features = HashMap::new();
                    features.insert(
                        ONION_MESSAGES_OPTIONAL,
                        tonic_lnd::lnrpc::Feature::default(),
                    );
                    features
                },
                ..Default::default()
            },
        ];

        mock_client
            .expect_list_peers_with_retries()
            .times(1)
            .returning(move |_| {
                Ok(Response::new(ListPeersResponse {
                    peers: mock_peers.clone(),
                }))
            });

        let notify_clone = reconnect_notification.clone();

        let task = tokio::spawn(async move {
            receive_reconnection_event(
                &mut mock_client,
                reconnect_notification.clone(),
                sender,
                listener,
            )
            .await
            .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        notify_clone.notify_one();
        let timeout_duration = Duration::from_millis(100);
        let mut received_events = Vec::new();
        for _ in 0..2 {
            match tokio::time::timeout(timeout_duration, receiver.recv()).await {
                Ok(Some(event)) => {
                    received_events.push(event);
                }
                Ok(None) => panic!("Channel closed unexpectedly"),
                Err(_) => panic!("Timed out waiting for events"),
            }
        }

        assert_eq!(received_events.len(), 2);
        let has_peer1_event = received_events.iter().any(|e| {
            if let MessengerEvents::PeerConnected(pk, onion_support) = e {
                *pk == pubkey1 && !*onion_support
            } else {
                false
            }
        });
        let has_peer2_event = received_events.iter().any(|e| {
            if let MessengerEvents::PeerConnected(pk, onion_support) = e {
                *pk == pubkey2 && *onion_support
            } else {
                false
            }
        });

        assert!(has_peer1_event, "Missing PeerConnected event for peer1");
        assert!(has_peer2_event, "Missing PeerConnected event for peer2");

        task.abort();
    }

    #[tokio::test]
    async fn test_consume_messenger_events() {
        let (sender, receiver) = channel(8);

        let pk_1 = pubkey(1);
        let pk_2 = pubkey(2);
        let mut mock = MockOnionHandler::new();
        let mut sender_mock = MockSendCustomMessenger::new();
        let mut rate_limiter = MockRateLimiter::new();

        // Setup rate limiter to no-op on peer connected / disconnected calls (we have proper
        // assertions for the onion messenger's calls anyway).
        rate_limiter.expect_peer_connected().returning(|_| {});
        rate_limiter.expect_peer_disconnected().returning(|_| {});

        // Peer connected: onion messaging supported.
        sender
            .send(MessengerEvents::PeerConnected(pk_2, true))
            .await
            .unwrap();

        mock.expect_peer_connected()
            .withf(|_: &PublicKey, init: &Init, _: &bool| init.features.supports_onion_messages())
            .return_once(|_, _, _| Ok(()));

        // Add two polling events for custom messages.
        sender.send(MessengerEvents::SendOutgoing).await.unwrap();
        sender.send(MessengerEvents::SendOutgoing).await.unwrap();

        rate_limiter
            .expect_peers()
            .returning(move || vec![pk_1, pk_2]);

        // Set up our mock to return an onion message for pk_1, and no onion messages for pk_2.
        mock.expect_next_onion_message_for_peer()
            .withf(move |actual_pk: &PublicKey| *actual_pk == pk_1)
            .returning(|_| Some(onion_message()));

        mock.expect_next_onion_message_for_peer()
            .returning(|_| None);

        sender_mock
            .expect_send_custom_message()
            .times(2)
            .returning(|_| Ok(SendCustomMessageResponse {}));

        // Peer connected: onion messaging not supported.
        sender
            .send(MessengerEvents::PeerConnected(pk_1, false))
            .await
            .unwrap();

        mock.expect_peer_connected()
            .withf(|_: &PublicKey, init: &Init, _: &bool| !init.features.supports_onion_messages())
            .return_once(|_, _, _| Ok(()));

        // Cover peer disconnected events.
        sender
            .send(MessengerEvents::PeerDisconnected(pk_1))
            .await
            .unwrap();
        mock.expect_peer_disconnected().return_once(|_| ());

        // Cover incoming onion messages - rate limiter allows incoming.
        let onion_message = onion_message();
        sender
            .send(MessengerEvents::IncomingMessage(
                pk_1,
                onion_message.clone(),
            ))
            .await
            .unwrap();

        rate_limiter
            .expect_query_peer()
            .withf(move |actual_pk: &PublicKey| *actual_pk == pk_1.clone())
            .returning(|_| true);
        mock.expect_handle_onion_message().return_once(|_, _| ());

        // Cover incoming onion messages - rate limiter disallows incoming.
        sender
            .send(MessengerEvents::IncomingMessage(pk_2, onion_message))
            .await
            .unwrap();
        rate_limiter
            .expect_query_peer()
            .withf(move |actual_pk: &PublicKey| *actual_pk == pk_2.clone())
            .returning(|_| false);

        // Finally, send a producer exit event to test exit.
        sender
            .send(MessengerEvents::ProducerExit(
                ConsumerError::PeerProducerExit,
            ))
            .await
            .unwrap();

        drop(sender);

        let consume_resp = consume_messenger_events(
            mock,
            receiver,
            &mut sender_mock,
            &mut rate_limiter,
            LndkEventHandler {
                lnd_client: MockTestPeerConnector::new(),
            },
            Network::Regtest,
        )
        .await;

        assert!(consume_resp.is_ok(), "the result should have been Ok()")
    }

    #[tokio::test]
    async fn test_consumer_exit_onion_messenger_failure() {
        let (sender, receiver) = channel(1);

        let pk = pubkey(0);
        let mut mock = MockOnionHandler::new();
        let mut rate_limiter = MockRateLimiter::new();

        // Send a peer connected event, but mock out an error on the handler's connected function.
        sender
            .send(MessengerEvents::PeerConnected(pk, true))
            .await
            .unwrap();
        mock.expect_peer_connected().return_once(|_, _, _| Err(()));

        let mut sender_mock = MockSendCustomMessenger::new();

        let consume_err = consume_messenger_events(
            mock,
            receiver,
            &mut sender_mock,
            &mut rate_limiter,
            LndkEventHandler {
                lnd_client: MockTestPeerConnector::new(),
            },
            Network::Regtest,
        )
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
        let mut sender_mock = MockSendCustomMessenger::new();
        let mut rate_limiter = MockRateLimiter::new();

        assert!(consume_messenger_events(
            MockOnionHandler::new(),
            receiver_done,
            &mut sender_mock,
            &mut rate_limiter,
            LndkEventHandler {
                lnd_client: MockTestPeerConnector::new()
            },
            Network::Regtest,
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn test_lndk_event_handler_connection_needed() {
        let mut connector_mock = MockTestPeerConnector::new();
        let expected_node_id = pubkey(1);

        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = notify.clone();

        connector_mock.expect_clone().times(1).returning(move || {
            let notify = notify_clone.clone();
            let mut mock = MockTestPeerConnector::new();

            mock.expect_list_peers()
                .returning(|| Ok(tonic_lnd::lnrpc::ListPeersResponse { peers: vec![] }));

            mock.expect_get_node_info().returning(|_, _| {
                let node_addr = tonic_lnd::lnrpc::NodeAddress {
                    network: String::from("regtest"),
                    addr: String::from("127.0.0.1:9735"),
                };
                let node = tonic_lnd::lnrpc::LightningNode {
                    addresses: vec![node_addr],
                    ..Default::default()
                };
                Ok(tonic_lnd::lnrpc::NodeInfo {
                    node: Some(node),
                    ..Default::default()
                })
            });

            mock.expect_connect_peer().times(1).returning(move |_, _| {
                notify.notify_one(); // Signal completion
                Ok(())
            });
            mock
        });

        let handler = LndkEventHandler {
            lnd_client: connector_mock,
        };

        let event = Event::ConnectionNeeded {
            node_id: expected_node_id,
            addresses: vec![],
        };

        let result = handler.handle_event(event);
        assert!(result.is_ok());
        tokio::select! {
            _ = notify.notified() => {
                // Successfully called
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(200)) => {
                panic!("Spawned connection should have completed within timeout");
            }
        }
    }

    #[tokio::test]
    async fn test_lndk_event_handler_other_events() {
        let connector_mock = MockTestPeerConnector::new();
        let handler = LndkEventHandler {
            lnd_client: connector_mock,
        };

        // Test that other events just return ok.
        let events = vec![
            Event::OnionMessageIntercepted {
                peer_node_id: pubkey(1),
                message: onion_message(),
            },
            Event::OnionMessagePeerConnected {
                peer_node_id: pubkey(2),
            },
        ];

        for event in events {
            let result = handler.handle_event(event);
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_lndk_event_handler_connection_failed() {
        let mut connector_mock = MockTestPeerConnector::new();
        let expected_node_id = pubkey(1);

        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_clone = notify.clone();

        connector_mock.expect_clone().times(1).returning(move || {
            let notify = notify_clone.clone();
            let mut mock = MockTestPeerConnector::new();

            mock.expect_list_peers()
                .returning(|| Ok(tonic_lnd::lnrpc::ListPeersResponse { peers: vec![] }));

            mock.expect_get_node_info().returning(|_, _| {
                let node_addr = tonic_lnd::lnrpc::NodeAddress {
                    network: String::from("regtest"),
                    addr: String::from("127.0.0.1:9735"),
                };
                let node = tonic_lnd::lnrpc::LightningNode {
                    addresses: vec![node_addr],
                    ..Default::default()
                };
                Ok(tonic_lnd::lnrpc::NodeInfo {
                    node: Some(node),
                    ..Default::default()
                })
            });

            mock.expect_connect_peer().times(1).returning(move |_, _| {
                notify.notify_one();
                Err(tonic_lnd::tonic::Status::unavailable("Connection failed"))
            });
            mock
        });

        let handler = LndkEventHandler {
            lnd_client: connector_mock,
        };

        let event = Event::ConnectionNeeded {
            node_id: expected_node_id,
            addresses: vec![],
        };

        let result = handler.handle_event(event);
        // Handler should still return Ok even if connection fails
        assert!(result.is_ok());

        tokio::select! {
            _ = notify.notified() => {
                // Successfully called
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_millis(200)) => {
                panic!("Spawned connection should have completed within timeout");
            }
        }
    }

    #[tokio::test]
    async fn test_produce_peer_events() {
        let (sender, mut receiver) = channel(4);
        let (_shutdown, listener) = triggered::trigger();

        let mut mock = MockPeerProducer::new();

        // Peer connects and we successfully lookup its support for onion messages.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey(0).to_string(),
                r#type: i32::from(PeerOnline),
            })
        });
        mock.expect_onion_support().times(1).returning(|_| true);

        // Peer connects with no onion support.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey(0).to_string(),
                r#type: i32::from(PeerOnline),
            })
        });
        mock.expect_onion_support().returning(|_| false);

        // Peer disconnects.
        mock.expect_receive().times(1).returning(|| {
            Ok(PeerEvent {
                pub_key: pubkey(0).to_string(),
                r#type: i32::from(PeerOffline),
            })
        });

        mock.expect_receive()
            .times(1)
            .returning(|| Err(Status::unknown("mock stream err")));

        matches!(
            produce_peer_events(mock, sender, listener)
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
        let (shutdown, listener) = triggered::trigger();

        let mock = MockPeerProducer::new();
        shutdown.trigger();
        assert!(produce_peer_events(mock, sender, listener).await.is_ok());
    }

    mock! {
        MessageProducer{}

        #[async_trait]
        impl IncomingMessageProducer for MessageProducer {
            async fn receive(&mut self) -> Result<CustomMessage, Status>;
        }
    }

    #[tokio::test]
    async fn test_produce_incoming_message_events() {
        let (sender, mut receiver) = channel(2);
        let (_shutdown, listener) = triggered::trigger();

        let mut mock = MockMessageProducer::new();

        // Send a custom message that is not relevant to us.
        mock.expect_receive().times(1).returning(|| {
            Ok(CustomMessage {
                peer: pubkey(0).serialize().to_vec(),
                r#type: 3,
                data: vec![1, 2, 3],
            })
        });

        // Send a custom message that is an onion message.
        mock.expect_receive().times(1).returning(|| {
            let mut w = vec![];
            onion_message().write(&mut w).unwrap();

            Ok(CustomMessage {
                peer: pubkey(0).serialize().to_vec(),
                r#type: ONION_MESSAGE_TYPE,
                data: w,
            })
        });

        mock.expect_receive()
            .times(1)
            .returning(|| Err(Status::unknown("mock stream err")));

        matches!(
            produce_incoming_message_events(mock, sender, listener)
                .await
                .expect_err("producer should error"),
            ProducerError::StreamError(_),
        );

        matches!(
            receiver.recv().await.unwrap(),
            MessengerEvents::IncomingMessage(_, _),
        );

        matches!(
            receiver.recv().await.unwrap(),
            MessengerEvents::ProducerExit(_)
        );
    }

    #[tokio::test]
    async fn test_produce_incoming_message_exit() {
        let (sender, _receiver) = channel(2);
        let (shutdown, listener) = triggered::trigger();

        let mock = MockMessageProducer::new();
        shutdown.trigger();
        assert!(produce_incoming_message_events(mock, sender, listener)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_produce_outgoing_message_events_exit() {
        let (sender, _) = channel(1);
        let (shutdown, listener) = triggered::trigger();
        let interval = time::interval(MSG_POLL_INTERVAL);

        // Let's test that produce_outgoing_message_events successfully exits when it receives the
        // signal, rather than loop infinitely.
        shutdown.trigger();
        assert!(produce_outgoing_message_events(sender, listener, interval)
            .await
            .is_ok());
    }
}
