use crate::lnd::{features_support_onion_messages, InvoicePayer, MessageSigner, PeerConnector};
use crate::{OfferHandler, PaymentState};
use async_trait::async_trait;
use bitcoin::hashes::Hmac;
use bitcoin::secp256k1::{PublicKey, Secp256k1};
use bitcoin::Network;
use lightning::blinded_path::message::{BlindedMessagePath, MessageContext, OffersContext};
use lightning::blinded_path::payment::BlindedPaymentPath;
use lightning::blinded_path::{Direction, IntroductionNode};
use lightning::ln::channelmanager::PaymentId;
use lightning::ln::channelmanager::Verification;
use lightning::offers::invoice_request::InvoiceRequest;
use lightning::offers::merkle::SignError;
use lightning::offers::nonce::Nonce;
use lightning::offers::offer::{Amount, Offer};
use lightning::offers::parse::{Bolt12ParseError, Bolt12SemanticError};
use lightning::onion_message::messenger::{Destination, MessageSendInstructions};
use lightning::onion_message::offers::OffersMessage;
use lightning::sign::EntropySource;
use log::{debug, error};
use std::collections::hash_map::Entry;
use std::error::Error;
use std::fmt::Display;
use std::str::FromStr;
use tonic_lnd::lnrpc::{
    ChanInfoRequest, GetInfoRequest, HtlcAttempt, ListPeersRequest, ListPeersResponse, NodeInfo,
    Payment, QueryRoutesResponse, Route,
};
use tonic_lnd::routerrpc::TrackPaymentRequest;
use tonic_lnd::signrpc::{KeyLocator, SignMessageReq};
use tonic_lnd::tonic::Status;
use tonic_lnd::Client;

#[derive(Debug)]
/// OfferError is an error that occurs during the process of paying an offer.
pub enum OfferError {
    /// AlreadyProcessing indicates that we're already trying to make a payment with the same id.
    AlreadyProcessing(PaymentId),
    /// BuildUIRFailure indicates a failure to build the unsigned invoice request.
    BuildUIRFailure(Bolt12SemanticError),
    /// SignError indicates a failure to sign the invoice request.
    SignError(SignError),
    /// DeriveKeyFailure indicates a failure to derive key for signing the invoice request.
    DeriveKeyFailure(Status),
    /// User provided an invalid amount.
    InvalidAmount(String),
    /// Invalid currency contained in the offer.
    InvalidCurrency,
    /// Unable to connect to peer.
    PeerConnectError(Status),
    /// No node address.
    NodeAddressNotFound,
    /// Cannot list peers.
    ListPeersFailure(Status),
    /// Failure to build a reply path.
    BuildBlindedPathFailure,
    /// Unable to find or send to payment route.
    RouteFailure(Status),
    /// Failed to track payment.
    TrackFailure(Status),
    /// Failed to send payment.
    PaymentFailure,
    /// Failed to receive an invoice back from offer creator before the timeout.
    InvoiceTimeout(u32),
    /// Failed to find introduction node for blinded path.
    IntroductionNodeNotFound,
    /// Cannot fetch channel info.
    GetChannelInfo(Status),
}

impl Display for OfferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfferError::AlreadyProcessing(id) => {
                write!(
                    f,
                    "We're already trying to pay for a payment with this id {id}"
                )
            }
            OfferError::BuildUIRFailure(e) => write!(f, "Error building invoice request: {e:?}"),
            OfferError::SignError(e) => write!(f, "Error signing invoice request: {e:?}"),
            OfferError::DeriveKeyFailure(e) => write!(f, "Error signing invoice request: {e:?}"),
            OfferError::InvalidAmount(e) => write!(f, "User provided an invalid amount: {e:?}"),
            OfferError::InvalidCurrency => write!(
                f,
                "LNDK doesn't yet support offer currencies other than bitcoin"
            ),
            OfferError::PeerConnectError(e) => write!(f, "Error connecting to peer: {e:?}"),
            OfferError::NodeAddressNotFound => write!(f, "Couldn't get node address"),
            OfferError::ListPeersFailure(e) => write!(f, "Error listing peers: {e:?}"),
            OfferError::BuildBlindedPathFailure => write!(f, "Error building blinded path"),
            OfferError::RouteFailure(e) => write!(f, "Error routing payment: {e:?}"),
            OfferError::TrackFailure(e) => write!(f, "Error tracking payment: {e:?}"),
            OfferError::PaymentFailure => write!(f, "Failed to send payment"),
            OfferError::InvoiceTimeout(e) => write!(f, "Did not receive invoice in {e:?} seconds."),
            OfferError::IntroductionNodeNotFound => write!(f, "Could not find introduction node."),
            OfferError::GetChannelInfo(e) => write!(f, "Could not fetch channel info: {e:?}"),
        }
    }
}

impl Error for OfferError {}

// Decodes a bech32 offer string into an LDK offer.
pub fn decode(offer_str: String) -> Result<Offer, Bolt12ParseError> {
    offer_str.parse::<Offer>()
}

impl OfferHandler {
    pub async fn send_invoice_request(
        &self,
        destination: Destination,
        mut client: Client,
        invoice_request: InvoiceRequest,
        offer_context: OffersContext,
    ) -> Result<(), OfferError> {
        // For now we connect directly to the introduction node of the blinded path so we don't need
        // any intermediate nodes here. In the future we'll query for a full path to the
        // introduction node for better sender privacy.
        match destination {
            Destination::Node(pubkey) => connect_to_peer(client.clone(), pubkey).await?,
            Destination::BlindedPath(ref path) => match path.introduction_node() {
                IntroductionNode::NodeId(pubkey) => {
                    connect_to_peer(client.clone(), *pubkey).await?
                }
                IntroductionNode::DirectedShortChannelId(direction, scid) => {
                    let pubkey = get_node_id(client.clone(), *scid, *direction).await?;
                    connect_to_peer(client.clone(), pubkey).await?
                }
            },
        };

        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .expect("failed to get info")
            .into_inner();

        let pubkey = PublicKey::from_str(&info.identity_pubkey).unwrap();
        let message_context = MessageContext::Offers(offer_context);
        let reply_path = Some(
            self.create_reply_path(client.clone(), pubkey, message_context)
                .await?,
        );

        if let Some(ref reply_path) = reply_path {
            let reply_path_intro_node_id = match reply_path.introduction_node() {
                IntroductionNode::NodeId(pubkey) => pubkey.to_string(),
                IntroductionNode::DirectedShortChannelId(direction, scid) => {
                    get_node_id(client.clone(), *scid, *direction)
                        .await?
                        .to_string()
                }
            };
            debug!(
                "In invoice request, we chose {} as the introduction node of the reply path",
                reply_path_intro_node_id
            );
        };

        let contents = OffersMessage::InvoiceRequest(invoice_request);
        let send_instructions = if let Some(reply_path_inner) = reply_path {
            MessageSendInstructions::WithSpecifiedReplyPath {
                destination,
                reply_path: reply_path_inner,
            }
        } else {
            MessageSendInstructions::WithoutReplyPath { destination }
        };

        let mut pending_messages = self.pending_messages.lock().unwrap();
        pending_messages.push((contents, send_instructions));
        std::mem::drop(pending_messages);

        Ok(())
    }

    // create_invoice_request builds and signs an invoice request, the first step in the BOLT 12
    // process of paying an offer.
    pub async fn create_invoice_request(
        &self,
        offer: Offer,
        network: Network,
        msats: Option<u64>,
        payer_note: Option<String>,
    ) -> Result<(InvoiceRequest, PaymentId, u64, OffersContext), OfferError> {
        let validated_amount = validate_amount(offer.amount().as_ref(), msats).await?;

        // Generate a default payment id for this payment.
        let payment_id = PaymentId(self.messenger_utils.get_secure_random_bytes());

        // We need to add some metadata to the invoice request to help with verification of the
        // invoice once returned from the offer maker. Once we get an invoice back, this metadata
        // will help us to determine: 1) That the invoice is truly for the invoice request we sent.
        // 2) We don't pay duplicate invoices.
        let secp_ctx = Secp256k1::new();
        let nonce = lightning::offers::nonce::Nonce::from_entropy_source(&self.messenger_utils);
        let builder = offer
            .request_invoice(&self.expanded_key, nonce, &secp_ctx, payment_id)
            .map_err(OfferError::BuildUIRFailure)?
            .chain(network)
            .map_err(OfferError::BuildUIRFailure)?
            .amount_msats(validated_amount)
            .map_err(OfferError::BuildUIRFailure)?;

        let builder = match payer_note {
            Some(payer_note_str) => builder.payer_note(payer_note_str),
            None => builder,
        };

        let invoice_request = builder
            .build_and_sign()
            .map_err(OfferError::BuildUIRFailure)?;

        {
            let mut active_payments = self.active_payments.lock().unwrap();
            match active_payments.entry(payment_id) {
                Entry::Occupied(_) => return Err(OfferError::AlreadyProcessing(payment_id)),
                Entry::Vacant(v) => {
                    v.insert(crate::PaymentInfo {
                        state: PaymentState::InvoiceRequestCreated,
                        invoice: None,
                    });
                }
            };
        }

        let hmac = payment_id.hmac_for_offer_payment(nonce, &self.expanded_key);
        let offer_context = OffersContext::OutboundPayment {
            payment_id,
            nonce,
            hmac: Some(hmac),
        };
        Ok((invoice_request, payment_id, validated_amount, offer_context))
    }

    /// create_reply_path creates a blinded path to provide to the offer node when requesting an
    /// invoice so they know where to send the invoice back to. We try to find a peer that we're
    /// connected to with the necessary requirements to form a blinded path. The peer needs two
    /// things:
    /// 1) Onion messaging support.
    /// 2) To be an advertised node with at least one public channel.
    ///
    /// Otherwise we create a blinded path directly to ourselves.
    pub async fn create_reply_path(
        &self,
        mut connector: impl PeerConnector + std::marker::Send + 'static,
        node_id: PublicKey,
        message_context: MessageContext,
    ) -> Result<BlindedMessagePath, OfferError> {
        // Find an introduction node for our blinded path.
        let current_peers = connector.list_peers().await.map_err(|e| {
            error!("Could not lookup current peers: {e}.");
            OfferError::ListPeersFailure(e)
        })?;

        let mut intro_node = None;
        for peer in current_peers.peers {
            let pubkey = PublicKey::from_str(&peer.pub_key).unwrap();
            let onion_support = features_support_onion_messages(&peer.features);
            if onion_support {
                // We also need to check that the candidate introduction node is actually an
                // advertised node with at least one public channel.
                match connector.get_node_info(peer.pub_key, true).await {
                    Ok(node) => {
                        if node.channels.is_empty() {
                            continue;
                        }
                    }
                    Err(_) => continue,
                };
                intro_node = Some(pubkey);
                break;
            }
        }

        let secp_ctx = Secp256k1::new();
        if intro_node.is_none() {
            Ok(BlindedMessagePath::one_hop(
                node_id,
                message_context,
                &self.messenger_utils,
                &secp_ctx,
            )
            .map_err(|_| {
                error!("Could not create blinded path.");
                OfferError::BuildBlindedPathFailure
            })?)
        } else {
            let nodes = vec![lightning::blinded_path::message::MessageForwardNode {
                node_id: intro_node.unwrap(),
                short_channel_id: None,
            }];
            Ok(BlindedMessagePath::new(
                &nodes,
                node_id,
                message_context,
                &self.messenger_utils,
                &secp_ctx,
            )
            .map_err(|_| {
                error!("Could not create blinded path.");
                OfferError::BuildBlindedPathFailure
            })?)
        }
    }

    /// send_payment tries to pay the provided invoice using LND.
    pub(crate) async fn send_payment(
        &self,
        mut payer: impl InvoicePayer + std::marker::Send + 'static,
        params: SendPaymentParams,
    ) -> Result<Payment, OfferError> {
        let resp = payer
            .query_routes(
                params.path,
                params.cltv_expiry_delta,
                params.fee_base_msat,
                params.fee_ppm,
                params.msats,
            )
            .await
            .map_err(OfferError::RouteFailure)?;

        let _ = payer
            .send_to_route(params.payment_hash, resp.routes[0].clone())
            .await
            .map_err(OfferError::RouteFailure)?;

        {
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments
                .entry(params.payment_id)
                .and_modify(|entry| entry.state = PaymentState::PaymentDispatched);
        }

        // We'll track the payment until it settles.
        payer
            .track_payment(params.payment_hash)
            .await
            .map_err(|_| OfferError::PaymentFailure)
    }

    /// Handles an invoice error for a payment ID.
    /// Verifies the payment ID and removes it from active payments if valid.
    pub fn handle_invoice_error(
        &self,
        payment_id: PaymentId,
        nonce: Nonce,
        hmac: Hmac<bitcoin::hashes::sha256::Hash>,
    ) {
        if let Ok(()) = payment_id.verify_for_offer_payment(hmac, nonce, &self.expanded_key) {
            error!("Received an invoice error for payment_id {payment_id}. Payment is abandoned.");
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments.remove(&payment_id);
        }
    }
}

pub struct SendPaymentParams {
    pub path: BlindedPaymentPath,
    pub cltv_expiry_delta: u16,
    pub fee_base_msat: u32,
    pub fee_ppm: u32,
    pub payment_hash: [u8; 32],
    pub msats: u64,
    pub payment_id: PaymentId,
}

/// Checks that the user-provided amount matches the provided offer or invoice.
///
/// Parameters:
///
/// * `offer_amount_msats`: The amount set in the offer or invoice.
/// * `amount_msats`: The amount we want to pay.
pub(crate) async fn validate_amount(
    offer_amount_msats: Option<&Amount>,
    pay_amount_msats: Option<u64>,
) -> Result<u64, OfferError> {
    let validated_amount = match offer_amount_msats {
        Some(offer_amount) => {
            match *offer_amount {
                Amount::Bitcoin {
                    amount_msats: bitcoin_amt,
                } => {
                    if let Some(msats) = pay_amount_msats {
                        if msats < bitcoin_amt {
                            return Err(OfferError::InvalidAmount(format!(
                                "{msats} is less than offer amount {}",
                                bitcoin_amt
                            )));
                        }
                        msats
                    } else {
                        // If user didn't set amount, set it to the offer amount.
                        if bitcoin_amt == 0 {
                            return Err(OfferError::InvalidAmount(
                                "Offer doesn't set an amount, so user must specify one".to_string(),
                            ));
                        }
                        bitcoin_amt
                    }
                }
                _ => {
                    return Err(OfferError::InvalidCurrency);
                }
            }
        }
        None => {
            if let Some(msats) = pay_amount_msats {
                msats
            } else {
                return Err(OfferError::InvalidAmount(
                    "Offer doesn't set an amount, so user must specify one".to_string(),
                ));
            }
        }
    };
    Ok(validated_amount)
}

pub async fn get_destination(offer: &Offer) -> Result<Destination, OfferError> {
    if offer.paths().is_empty() {
        if let Some(signing_pubkey) = offer.issuer_signing_pubkey() {
            Ok(Destination::Node(signing_pubkey))
        } else {
            Err(OfferError::IntroductionNodeNotFound)
        }
    } else {
        Ok(Destination::BlindedPath(offer.paths()[0].clone()))
    }
}

// connect_to_peer connects to the provided node if we're not already connected.
pub async fn connect_to_peer(
    mut connector: impl PeerConnector,
    node_id: PublicKey,
) -> Result<(), OfferError> {
    let resp = connector
        .list_peers()
        .await
        .map_err(OfferError::PeerConnectError)?;

    let node_id_str = node_id.to_string();
    for peer in resp.peers.iter() {
        if peer.pub_key == node_id_str {
            return Ok(());
        }
    }

    debug!("Connecting to peer: {}", node_id_str);
    let node = connector
        .get_node_info(node_id_str.clone(), false)
        .await
        .map_err(OfferError::PeerConnectError)?;

    let node = match node.node {
        Some(node) => node,
        None => return Err(OfferError::NodeAddressNotFound),
    };

    if node.addresses.is_empty() {
        return Err(OfferError::NodeAddressNotFound);
    }

    debug!(
        "Connecting to peer address: {}",
        node.addresses[0].clone().addr
    );
    connector
        .connect_peer(node_id_str, node.addresses[0].clone().addr)
        .await
        .map_err(OfferError::PeerConnectError)?;

    Ok(())
}

#[async_trait]
impl PeerConnector for Client {
    async fn list_peers(&mut self) -> Result<ListPeersResponse, Status> {
        let list_req = ListPeersRequest {
            ..Default::default()
        };
        self.lightning()
            .list_peers(list_req)
            .await
            .map(|resp| resp.into_inner())
    }

    async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status> {
        let ln_addr = tonic_lnd::lnrpc::LightningAddress {
            pubkey: node_id,
            host: addr,
        };

        let connect_req = tonic_lnd::lnrpc::ConnectPeerRequest {
            addr: Some(ln_addr),
            timeout: 20,
            ..Default::default()
        };

        self.lightning()
            .connect_peer(connect_req.clone())
            .await
            .map(|_| ())
    }

    async fn get_node_info(
        &mut self,
        pub_key: String,
        include_channels: bool,
    ) -> Result<NodeInfo, Status> {
        let req = tonic_lnd::lnrpc::NodeInfoRequest {
            pub_key,
            include_channels,
        };

        self.lightning()
            .get_node_info(req)
            .await
            .map(|resp| resp.into_inner())
    }
}

#[async_trait]
impl MessageSigner for Client {
    async fn sign_message(
        &mut self,
        msg: &[u8],
        key_loc: KeyLocator,
        double_hash: bool,
        schnorr_sig: bool,
    ) -> Result<Vec<u8>, ()> {
        let req = SignMessageReq {
            msg: msg.to_vec(),
            key_loc: Some(key_loc),
            double_hash,
            schnorr_sig,
            ..Default::default()
        };

        let resp = self
            .signer()
            .sign_message(req)
            .await
            .expect("Failed to sign message");
        let resp_inner = resp.into_inner();
        Ok(resp_inner.signature)
    }
}

#[async_trait]
impl InvoicePayer for Client {
    async fn query_routes(
        &mut self,
        path: BlindedPaymentPath,
        cltv_expiry_delta: u16,
        fee_base_msat: u32,
        fee_ppm: u32,
        msats: u64,
    ) -> Result<QueryRoutesResponse, Status> {
        let mut blinded_hops = vec![];
        for hop in path.blinded_hops().iter() {
            let new_hop = tonic_lnd::lnrpc::BlindedHop {
                blinded_node: hop.blinded_node_id.serialize().to_vec(),
                encrypted_data: hop.clone().encrypted_payload,
            };
            blinded_hops.push(new_hop);
        }

        // We'll need to store the pubkey outside the match to fix lifetime issues
        let node_id_from_scid;
        let introduction_node = match path.introduction_node() {
            IntroductionNode::NodeId(pubkey) => pubkey,
            IntroductionNode::DirectedShortChannelId(direction, scid) => {
                node_id_from_scid =
                    get_node_id(self.clone(), *scid, *direction)
                        .await
                        .map_err(|e| {
                            error!("{e}");
                            Status::unknown("Could not get node id.")
                        })?;

                // Using the longer-lived reference
                &node_id_from_scid
            }
        };
        let blinded_path = Some(tonic_lnd::lnrpc::BlindedPath {
            introduction_node: introduction_node.serialize().to_vec(),
            blinding_point: path.blinding_point().serialize().to_vec(),
            blinded_hops,
        });

        let blinded_payment_paths = tonic_lnd::lnrpc::BlindedPaymentPath {
            blinded_path,
            total_cltv_delta: u32::from(cltv_expiry_delta) + 120,
            base_fee_msat: u64::from(fee_base_msat),
            proportional_fee_rate: fee_ppm,
            ..Default::default()
        };

        let query_req = tonic_lnd::lnrpc::QueryRoutesRequest {
            amt_msat: msats as i64,
            blinded_payment_paths: vec![blinded_payment_paths],
            ..Default::default()
        };

        let resp = self.lightning().query_routes(query_req).await?;
        Ok(resp.into_inner())
    }

    async fn send_to_route(
        &mut self,
        payment_hash: [u8; 32],
        route: Route,
    ) -> Result<HtlcAttempt, Status> {
        let send_req = tonic_lnd::routerrpc::SendToRouteRequest {
            payment_hash: payment_hash.to_vec(),
            route: Some(route),
            ..Default::default()
        };

        let resp = self.router().send_to_route_v2(send_req).await?;
        Ok(resp.into_inner())
    }

    async fn track_payment(&mut self, payment_hash: [u8; 32]) -> Result<Payment, OfferError> {
        let req = TrackPaymentRequest {
            payment_hash: payment_hash.to_vec(),
            no_inflight_updates: true,
        };

        let mut stream = self
            .router()
            .track_payment_v2(req)
            .await
            .map_err(OfferError::TrackFailure)?
            .into_inner();

        // Wait for a failed or successful payment.
        while let Some(payment) = stream.message().await.map_err(OfferError::TrackFailure)? {
            if payment.status() == tonic_lnd::lnrpc::payment::PaymentStatus::Succeeded {
                return Ok(payment);
            } else if payment.status() == tonic_lnd::lnrpc::payment::PaymentStatus::Failed {
                return Err(OfferError::PaymentFailure);
            } else {
                continue;
            }
        }

        Err(OfferError::PaymentFailure)
    }
}

// get_node_id finds an introduction node from the scid and direction provided by a blinded path.
pub(crate) async fn get_node_id(
    client: Client,
    scid: u64,
    direction: Direction,
) -> Result<PublicKey, OfferError> {
    let get_info_request = ChanInfoRequest {
        chan_id: scid,
        chan_point: "".to_string(),
    };
    let channel_info = client
        .lightning_read_only()
        .get_chan_info(get_info_request)
        .await
        .map_err(OfferError::GetChannelInfo)?
        .into_inner();
    let pubkey = match direction {
        Direction::NodeOne => channel_info.node1_pub,
        Direction::NodeTwo => channel_info.node2_pub,
    };
    PublicKey::from_slice(pubkey.as_bytes()).map_err(|e| {
        error!("Could not parse pubkey. {e}");
        OfferError::IntroductionNodeNotFound
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MessengerUtilities;
    use crate::PaymentInfo;
    use crate::PaymentState;
    use bitcoin::key::Keypair;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning::blinded_path::payment::{
        Bolt12OfferContext, ForwardTlvs, PaymentConstraints, PaymentContext, PaymentForwardNode,
        PaymentRelay, UnauthenticatedReceiveTlvs,
    };
    use lightning::bolt11_invoice::PaymentSecret;
    use lightning::ln::inbound_payment::ExpandedKey;
    use lightning::offers::invoice_request::InvoiceRequestFields;
    use lightning::offers::nonce::Nonce;
    use lightning::offers::offer::{OfferBuilder, OfferId, Quantity};
    use lightning::types::features::BlindedHopFeatures;
    use lightning::util::string::UntrustedString;
    use mockall::mock;
    use mockall::predicate::eq;
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::time::{Duration, SystemTime};
    use tonic_lnd::lnrpc::{ChannelEdge, LightningNode, NodeAddress, Payment};

    const NONCE_BYTES: &[u8] = &[42u8; 16];

    fn get_message_context() -> MessageContext {
        let offer_context = OffersContext::OutboundPayment {
            payment_id: PaymentId([42; 32]),
            nonce: Nonce::try_from(NONCE_BYTES).unwrap(),
            hmac: None,
        };
        MessageContext::Offers(offer_context)
    }

    fn get_offer() -> String {
        "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzrcgqgn3qzsyvfkx26qkyypvr5hfx60h9w9k934lt8s2n6zc0wwtgqlulw7dythr83dqx8tzumg".to_string()
    }

    fn build_custom_offer(amount_msats: u64) -> Offer {
        let secp_ctx = Secp256k1::new();
        let keys = Keypair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap());
        let pubkey = PublicKey::from(keys);

        let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
        OfferBuilder::new(pubkey)
            .description("coffee".to_string())
            .amount_msats(amount_msats)
            .supported_quantity(Quantity::Unbounded)
            .absolute_expiry(expiration.duration_since(SystemTime::UNIX_EPOCH).unwrap())
            .issuer("Foo Bar".to_string())
            .build()
            .unwrap()
    }

    fn get_pubkeys() -> Vec<String> {
        let pubkey1 =
            "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba".to_string();
        let pubkey2 =
            "03b060a3b572ab060532fbe49506fe25b5957195733788aab01ab3c0f40bb52602".to_string();
        vec![pubkey1, pubkey2]
    }

    fn get_blinded_payment_path() -> BlindedPaymentPath {
        let entropy_source = MessengerUtilities::new([42; 32]);
        let secp_ctx = Secp256k1::new();
        let nonce = Nonce::try_from(NONCE_BYTES).unwrap();
        let node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let expanded_key = ExpandedKey::new([42; 32]);
        let payment_context = PaymentContext::Bolt12Offer(Bolt12OfferContext {
            offer_id: OfferId([42; 32]),
            invoice_request: InvoiceRequestFields {
                payer_signing_pubkey: PublicKey::from_str(&get_pubkeys()[0]).unwrap(),
                quantity: Some(1),
                payer_note_truncated: Some(UntrustedString("".to_string())),
                human_readable_name: None,
            },
        });
        let payee_tlvs = UnauthenticatedReceiveTlvs {
            payment_secret: PaymentSecret([42; 32]),
            payment_constraints: PaymentConstraints {
                max_cltv_expiry: 1_000_000,
                htlc_minimum_msat: 1,
            },
            payment_context,
        };
        let payee_tlvs = payee_tlvs.authenticate(nonce, &expanded_key);
        let intermediate_nodes = [PaymentForwardNode {
            tlvs: ForwardTlvs {
                short_channel_id: 43,
                payment_relay: PaymentRelay {
                    cltv_expiry_delta: 40,
                    fee_proportional_millionths: 1_000,
                    fee_base_msat: 1,
                },
                payment_constraints: PaymentConstraints {
                    max_cltv_expiry: payee_tlvs.tlvs().payment_constraints.max_cltv_expiry + 40,
                    htlc_minimum_msat: 100,
                },
                features: BlindedHopFeatures::empty(),
                next_blinding_override: None,
            },
            node_id: node_id,
            htlc_maximum_msat: 1_000_000_000_000,
        }];
        let payment_path = BlindedPaymentPath::new(
            &intermediate_nodes,
            node_id,
            payee_tlvs,
            u64::MAX,
            12344,
            &entropy_source,
            &secp_ctx,
        )
        .unwrap();
        payment_path
    }

    mock! {
        TestPeerConnector{}

         #[async_trait]
         impl PeerConnector for TestPeerConnector {
             async fn list_peers(&mut self) -> Result<ListPeersResponse, Status>;
             async fn get_node_info(&mut self, pub_key: String, include_channels: bool) -> Result<NodeInfo, Status>;
             async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
         }
    }

    mock! {
    TestInvoicePayer{}

        #[async_trait]
        impl InvoicePayer for TestInvoicePayer{
            async fn query_routes(&mut self, path: BlindedPaymentPath, cltv_expiry_delta: u16, fee_base_msat: u32, fee_ppm: u32, msats: u64) -> Result<QueryRoutesResponse, Status>;
            async fn send_to_route(&mut self, payment_hash: [u8; 32], route: Route) -> Result<HtlcAttempt, Status>;
            async fn track_payment(&mut self, payment_hash: [u8; 32]) -> Result<Payment, OfferError>;
        }
    }

    #[tokio::test]
    async fn test_request_invoice() {
        let offer = decode(get_offer()).unwrap();
        let offer_amount = offer.amount().unwrap();
        let amount = match offer_amount {
            Amount::Bitcoin { amount_msats } => amount_msats,
            _ => panic!("unexpected amount type"),
        };
        let offer = decode(get_offer()).unwrap();
        let handler = OfferHandler::default();
        let resp = handler
            .create_invoice_request(offer, Network::Regtest, Some(amount), Some("".to_string()))
            .await;
        assert!(resp.is_ok())
    }

    #[tokio::test]
    async fn test_validate_amount() {
        // If the amount the user provided is greater than the offer-provided amount, then
        // we should be good.
        let offer = build_custom_offer(20000);
        let offer_amount = offer.amount();
        assert!(validate_amount(offer_amount.as_ref(), Some(20000))
            .await
            .is_ok());

        let offer = build_custom_offer(0);
        let offer_amount = offer.amount();
        assert!(validate_amount(offer_amount.as_ref(), Some(20000))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_validate_invalid_amount() {
        // If the amount the user provided is lower than the offer amount, we error.
        let offer = build_custom_offer(20000);
        let offer_amount = offer.amount();
        assert!(validate_amount(offer_amount.as_ref(), Some(1000))
            .await
            .is_err());

        // Both user amount and offer amount can't be 0.
        let offer = build_custom_offer(0);
        let offer_amount = offer.amount();
        assert!(validate_amount(offer_amount.as_ref(), None).await.is_err());
    }

    #[tokio::test]
    async fn test_connect_peer() {
        let mut connector_mock = MockTestPeerConnector::new();
        connector_mock.expect_list_peers().returning(|| {
            Ok(ListPeersResponse {
                ..Default::default()
            })
        });

        connector_mock.expect_get_node_info().returning(|_, _| {
            let node_addr = NodeAddress {
                network: String::from("regtest"),
                addr: String::from("127.0.0.1"),
            };
            let node = Some(LightningNode {
                addresses: vec![node_addr],
                ..Default::default()
            });

            let node_info = NodeInfo {
                node,
                ..Default::default()
            };

            Ok(node_info)
        });

        connector_mock
            .expect_connect_peer()
            .returning(|_, _| Ok(()));

        let pubkey = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        assert!(connect_to_peer(connector_mock, pubkey).await.is_ok());
    }

    #[tokio::test]
    async fn test_connect_peer_already_connected() {
        let mut connector_mock = MockTestPeerConnector::new();
        connector_mock.expect_list_peers().returning(|| {
            let peer = tonic_lnd::lnrpc::Peer {
                pub_key: get_pubkeys()[0].clone(),
                ..Default::default()
            };

            Ok(ListPeersResponse {
                peers: vec![peer],
                ..Default::default()
            })
        });

        connector_mock.expect_get_node_info().returning(|_, _| {
            let node_addr = NodeAddress {
                network: String::from("regtest"),
                addr: String::from("127.0.0.1"),
            };
            let node = Some(LightningNode {
                addresses: vec![node_addr],
                ..Default::default()
            });

            Ok(NodeInfo {
                node,
                ..Default::default()
            })
        });

        let pubkey = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        assert!(connect_to_peer(connector_mock, pubkey).await.is_ok());
    }

    #[tokio::test]
    async fn test_connect_peer_connect_error() {
        let mut connector_mock = MockTestPeerConnector::new();
        connector_mock.expect_list_peers().returning(|| {
            Ok(ListPeersResponse {
                ..Default::default()
            })
        });

        connector_mock.expect_get_node_info().returning(|_, _| {
            let node_addr = NodeAddress {
                network: String::from("regtest"),
                addr: String::from("127.0.0.1"),
            };
            let node = Some(LightningNode {
                addresses: vec![node_addr],
                ..Default::default()
            });

            Ok(NodeInfo {
                node,
                ..Default::default()
            })
        });

        connector_mock
            .expect_connect_peer()
            .returning(|_, _| Err(Status::unknown("")));

        let pubkey = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        assert!(connect_to_peer(connector_mock, pubkey).await.is_err());
    }

    #[tokio::test]
    async fn test_create_reply_path() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock.expect_list_peers().returning(|| {
            let feature = tonic_lnd::lnrpc::Feature {
                ..Default::default()
            };
            let mut feature_entry = HashMap::new();
            feature_entry.insert(38, feature);

            let peer = tonic_lnd::lnrpc::Peer {
                pub_key: get_pubkeys()[0].clone(),
                features: feature_entry,
                ..Default::default()
            };
            Ok(ListPeersResponse { peers: vec![peer] })
        });

        connector_mock.expect_get_node_info().returning(|_, _| {
            let node = Some(LightningNode {
                ..Default::default()
            });

            Ok(NodeInfo {
                node,
                ..Default::default()
            })
        });
        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();

        let message_context = get_message_context();
        let handler = OfferHandler::default();
        let reply_path = handler
            .create_reply_path(connector_mock, receiver_node_id, message_context)
            .await;
        assert!(reply_path.is_ok());
    }

    #[tokio::test]
    // Test that create_reply_path works fine when no suitable introduction node peer is found.
    async fn test_create_reply_path_no_intro_node() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_peers()
            .returning(|| Ok(ListPeersResponse { peers: vec![] }));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let handler = OfferHandler::default();
        assert!(handler
            .create_reply_path(connector_mock, receiver_node_id, message_context)
            .await
            .is_ok())
    }

    #[tokio::test]
    async fn test_create_reply_path_list_peers_error() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_peers()
            .returning(|| Err(Status::unknown("unknown error")));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let handler = OfferHandler::default();
        assert!(handler
            .create_reply_path(connector_mock, receiver_node_id, message_context)
            .await
            .is_err())
    }

    #[tokio::test]
    async fn test_create_reply_path_not_advertised() {
        // First lets test that if we're only connected to one peer. It has onion support, but the
        // node isn't advertised, meaning it has no public channels. This should return
        // a blinded path with only one hop.
        let mut connector_mock = MockTestPeerConnector::new();
        connector_mock.expect_list_peers().returning(|| {
            let feature = tonic_lnd::lnrpc::Feature {
                ..Default::default()
            };
            let mut feature_entry = HashMap::new();
            feature_entry.insert(38, feature);

            let peer = tonic_lnd::lnrpc::Peer {
                pub_key: get_pubkeys()[0].clone(),
                features: feature_entry,
                ..Default::default()
            };
            Ok(ListPeersResponse { peers: vec![peer] })
        });

        connector_mock
            .expect_get_node_info()
            .returning(|_, _| Err(Status::not_found("node was not found")));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let handler = OfferHandler::default();
        let resp = handler
            .create_reply_path(connector_mock, receiver_node_id, message_context)
            .await;
        assert!(resp.is_ok());
        let reply_path = resp.unwrap();
        let hops = reply_path.blinded_hops();
        assert!(hops.len() == 1);

        // Now let's test that we have two peers that both have onion support feature flags set.
        // One isn't advertised (i.e. it has no public channels). But the second is. This
        // should succeed.
        let mut connector_mock = MockTestPeerConnector::new();
        connector_mock.expect_list_peers().returning(|| {
            let feature = tonic_lnd::lnrpc::Feature {
                ..Default::default()
            };
            let mut feature_entry = HashMap::new();
            feature_entry.insert(38, feature);

            let keys = get_pubkeys();

            let peer1 = tonic_lnd::lnrpc::Peer {
                pub_key: keys[0].clone(),
                features: feature_entry.clone(),
                ..Default::default()
            };
            let peer2 = tonic_lnd::lnrpc::Peer {
                pub_key: keys[1].clone(),
                features: feature_entry,
                ..Default::default()
            };
            Ok(ListPeersResponse {
                peers: vec![peer1, peer2],
            })
        });

        let keys = get_pubkeys();
        connector_mock
            .expect_get_node_info()
            .with(eq(keys[0].clone()), eq(true))
            .returning(|_, _| Err(Status::not_found("node was not found")));

        connector_mock
            .expect_get_node_info()
            .with(eq(keys[1].clone()), eq(true))
            .returning(|_, _| {
                let node = Some(LightningNode {
                    ..Default::default()
                });

                Ok(NodeInfo {
                    node,
                    channels: vec![ChannelEdge {
                        ..Default::default()
                    }],
                    ..Default::default()
                })
            });

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let handler = OfferHandler::default();
        let message_context = get_message_context();
        let resp = handler
            .create_reply_path(connector_mock, receiver_node_id, message_context)
            .await;
        assert!(resp.is_ok());
        let reply_path = resp.unwrap();
        let hops = reply_path.blinded_hops();
        assert!(hops.len() == 2);
    }

    #[tokio::test]
    async fn test_send_payment() {
        let mut payer_mock = MockTestInvoicePayer::new();

        payer_mock.expect_query_routes().returning(|_, _, _, _, _| {
            let route = Route {
                ..Default::default()
            };
            Ok(QueryRoutesResponse {
                routes: vec![route],
                ..Default::default()
            })
        });

        payer_mock.expect_send_to_route().returning(|_, _| {
            Ok(HtlcAttempt {
                ..Default::default()
            })
        });

        payer_mock.expect_track_payment().returning(|_| {
            Ok(Payment {
                ..Default::default()
            })
        });

        let blinded_path = get_blinded_payment_path();
        let payment_hash = MessengerUtilities::default().get_secure_random_bytes();
        let handler = OfferHandler::default();
        let payment_id = PaymentId(MessengerUtilities::default().get_secure_random_bytes());
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash: payment_hash,
            msats: 2000,
            payment_id,
        };
        assert!(handler.send_payment(payer_mock, params).await.is_ok());
    }

    #[tokio::test]
    async fn test_send_payment_query_error() {
        let mut payer_mock = MockTestInvoicePayer::new();

        payer_mock
            .expect_query_routes()
            .returning(|_, _, _, _, _| Err(Status::unknown("unknown error")));

        let blinded_path = get_blinded_payment_path();
        let payment_hash = MessengerUtilities::default().get_secure_random_bytes();
        let payment_id = PaymentId(MessengerUtilities::default().get_secure_random_bytes());
        let handler = OfferHandler::default();
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash: payment_hash,
            msats: 2000,
            payment_id,
        };
        assert!(handler.send_payment(payer_mock, params).await.is_err());
    }

    #[tokio::test]
    async fn test_send_payment_send_error() {
        let mut payer_mock = MockTestInvoicePayer::new();

        payer_mock.expect_query_routes().returning(|_, _, _, _, _| {
            let route = Route {
                ..Default::default()
            };
            Ok(QueryRoutesResponse {
                routes: vec![route],
                ..Default::default()
            })
        });

        payer_mock
            .expect_send_to_route()
            .returning(|_, _| Err(Status::unknown("unknown error")));

        let blinded_path = get_blinded_payment_path();
        let payment_hash = MessengerUtilities::default().get_secure_random_bytes();
        let payment_id = PaymentId(MessengerUtilities::default().get_secure_random_bytes());
        let handler = OfferHandler::default();
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash: payment_hash,
            msats: 2000,
            payment_id,
        };
        assert!(handler.send_payment(payer_mock, params).await.is_err());
    }

    #[test]
    fn test_handle_invoice_error_existing_payment() {
        // Create an OfferHandler with a payment ID in active_payments
        let handler = OfferHandler::default();
        let payment_id = PaymentId([42; 32]);
        let nonce = Nonce::try_from(NONCE_BYTES).unwrap();

        // Insert a payment into active_payments
        {
            let mut active_payments = handler.active_payments.lock().unwrap();
            active_payments.insert(
                payment_id,
                PaymentInfo {
                    state: PaymentState::InvoiceRequestSent,
                    invoice: None,
                },
            );
        }

        // Calculate a valid HMAC
        let hmac = payment_id.hmac_for_offer_payment(nonce, &handler.expanded_key);

        // Call handle_invoice_error
        handler.handle_invoice_error(payment_id, nonce, hmac);

        // Verify payment was removed
        let active_payments = handler.active_payments.lock().unwrap();
        assert!(!active_payments.contains_key(&payment_id));
    }

    #[test]
    fn test_handle_invoice_error_nonexistent_payment() {
        // Create an OfferHandler with an empty active_payments
        let handler = OfferHandler::default();
        let payment_id = PaymentId([42; 32]);
        let nonce = Nonce::try_from(NONCE_BYTES).unwrap();

        // Calculate a valid HMAC
        let hmac = payment_id.hmac_for_offer_payment(nonce, &handler.expanded_key);

        // Call handle_invoice_error and nothing should happen.
        handler.handle_invoice_error(payment_id, nonce, hmac);
    }
}
