use crate::lnd::{features_support_onion_messages, InvoicePayer, MessageSigner, PeerConnector};
use crate::{OfferHandler, PaymentState};
use async_trait::async_trait;
use bitcoin::hashes::sha256::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::schnorr::Signature;
use bitcoin::secp256k1::{PublicKey, Secp256k1};
use futures::executor::block_on;
use lightning::blinded_path::{BlindedPath, Direction, IntroductionNode};
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::invoice_request::{
    InvoiceRequest, SignInvoiceRequestFn, UnsignedInvoiceRequest,
};
use lightning::offers::merkle::SignError;
use lightning::offers::offer::{Amount, Offer};
use lightning::offers::parse::{Bolt12ParseError, Bolt12SemanticError};
use lightning::onion_message::messenger::{Destination, PendingOnionMessage};
use lightning::onion_message::offers::OffersMessage;
use lightning::sign::EntropySource;
use log::{debug, error};
use std::collections::hash_map::Entry;
use std::error::Error;
use std::fmt::Display;
use std::str::FromStr;
use tokio::task;
use tonic_lnd::lnrpc::{
    ChanInfoRequest, GetInfoRequest, HtlcAttempt, LightningNode, ListPeersRequest,
    ListPeersResponse, Payment, QueryRoutesResponse, Route,
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
    InvoiceTimeout,
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
            OfferError::InvoiceTimeout => write!(f, "Did not receive invoice in 100 seconds."),
            OfferError::IntroductionNodeNotFound => write!(f, "Could not find introduction node."),
            OfferError::GetChannelInfo(e) => write!(f, "Could not fetch channel info: {e:?}"),
        }
    }
}

impl Error for OfferError {}

// Decodes a bech32 string into an LDK offer.
pub fn decode(offer_str: String) -> Result<Offer, Bolt12ParseError> {
    offer_str.parse::<Offer>()
}

impl OfferHandler {
    pub async fn send_invoice_request(
        &self,
        destination: Destination,
        mut client: Client,
        mut reply_path: Option<BlindedPath>,
        invoice_request: InvoiceRequest,
    ) -> Result<(), OfferError> {
        // For now we connect directly to the introduction node of the blinded path so we don't need
        // any intermediate nodes here. In the future we'll query for a full path to the
        // introduction node for better sender privacy.
        match destination {
            Destination::Node(pubkey) => connect_to_peer(client.clone(), pubkey).await?,
            Destination::BlindedPath(ref path) => match path.introduction_node {
                IntroductionNode::NodeId(pubkey) => connect_to_peer(client.clone(), pubkey).await?,
                IntroductionNode::DirectedShortChannelId(direction, scid) => {
                    let pubkey = get_node_id(client.clone(), scid, direction).await?;
                    connect_to_peer(client.clone(), pubkey).await?
                }
            },
        };

        if reply_path.is_none() {
            let info = client
                .lightning()
                .get_info(GetInfoRequest {})
                .await
                .expect("failed to get info")
                .into_inner();

            let pubkey = PublicKey::from_str(&info.identity_pubkey).unwrap();
            reply_path = Some(self.create_reply_path(client.clone(), pubkey).await?)
        };

        if let Some(ref reply_path) = reply_path {
            let reply_path_intro_node_id = match reply_path.introduction_node {
                IntroductionNode::NodeId(pubkey) => pubkey.to_string(),
                IntroductionNode::DirectedShortChannelId(direction, scid) => {
                    get_node_id(client.clone(), scid, direction)
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
        let pending_message = PendingOnionMessage {
            contents,
            destination,
            reply_path,
        };

        let mut pending_messages = self.pending_messages.lock().unwrap();
        pending_messages.push(pending_message);
        std::mem::drop(pending_messages);

        Ok(())
    }

    // create_invoice_request builds and signs an invoice request, the first step in the BOLT 12
    // process of paying an offer.
    pub async fn create_invoice_request(
        &self,
        mut signer: impl MessageSigner + std::marker::Send + 'static,
        offer: Offer,
        _metadata: Vec<u8>,
        network: Network,
        msats: Option<u64>,
    ) -> Result<(InvoiceRequest, PaymentId, u64), OfferError> {
        let validated_amount = validate_amount(offer.amount(), msats).await?;

        // We use KeyFamily KeyFamilyNodeKey (6) to derive a key to represent our node id. See:
        // https://github.com/lightningnetwork/lnd/blob/a3f8011ed695f6204ec6a13ad5c2a67ac542b109/keychain/derivation.go#L103
        let key_loc = KeyLocator {
            key_family: 6,
            key_index: 1,
        };

        let pubkey_bytes = signer
            .derive_key(key_loc.clone())
            .await
            .map_err(OfferError::DeriveKeyFailure)?;
        let pubkey =
            PublicKey::from_slice(&pubkey_bytes).expect("failed to deserialize public key");

        // Generate a new payment id for this payment.
        let payment_id = PaymentId(self.messenger_utils.get_secure_random_bytes());

        // We need to add some metadata to the invoice request to help with verification of the
        // invoice once returned from the offer maker. Once we get an invoice back, this metadata
        // will help us to determine: 1) That the invoice is truly for the invoice request we sent.
        // 2) We don't pay duplicate invoices.
        let unsigned_invoice_req = offer
            .request_invoice_deriving_metadata(
                pubkey,
                &self.expanded_key,
                &self.messenger_utils,
                payment_id,
            )
            .unwrap()
            .chain(network)
            .unwrap()
            .amount_msats(validated_amount)
            .unwrap()
            .build()
            .map_err(OfferError::BuildUIRFailure)?;

        // To create a valid invoice request, we also need to sign it. This is spawned in a blocking
        // task because we need to call block_on on sign_message so that sign_closure can be a
        // synchronous closure.
        let invoice_request =
            task::spawn_blocking(move || signer.sign_uir(key_loc, unsigned_invoice_req))
                .await
                .unwrap()?;

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

        Ok((invoice_request, payment_id, validated_amount))
    }

    /// create_reply_path creates a blinded path to provide to the offer maker when requesting an
    /// invoice so they know where to send the invoice back to. We try to find a peer that we're
    /// connected to with onion messaging support that we can use to form a blinded path,
    /// otherwise we creae a blinded path directly to ourselves.
    pub async fn create_reply_path(
        &self,
        mut connector: impl PeerConnector + std::marker::Send + 'static,
        node_id: PublicKey,
    ) -> Result<BlindedPath, OfferError> {
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
                intro_node = Some(pubkey);
            }
        }

        let secp_ctx = Secp256k1::new();
        if intro_node.is_none() {
            Ok(
                BlindedPath::one_hop_for_message(node_id, &self.messenger_utils, &secp_ctx)
                    .map_err(|_| {
                        error!("Could not create blinded path.");
                        OfferError::BuildBlindedPathFailure
                    })?,
            )
        } else {
            Ok(BlindedPath::new_for_message(
                &[intro_node.unwrap(), node_id],
                &self.messenger_utils,
                &secp_ctx,
            )
            .map_err(|_| {
                error!("Could not create blinded path.");
                OfferError::BuildBlindedPathFailure
            }))?
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
}

pub struct SendPaymentParams {
    pub path: BlindedPath,
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
        if let Some(signing_pubkey) = offer.signing_pubkey() {
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

    let node = connector
        .get_node_info(node_id_str.clone())
        .await
        .map_err(OfferError::PeerConnectError)?;

    let node = match node {
        Some(node) => node,
        None => return Err(OfferError::NodeAddressNotFound),
    };

    if node.addresses.is_empty() {
        return Err(OfferError::NodeAddressNotFound);
    }

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

    async fn get_node_info(&mut self, pub_key: String) -> Result<Option<LightningNode>, Status> {
        let req = tonic_lnd::lnrpc::NodeInfoRequest {
            pub_key,
            include_channels: false,
        };

        self.lightning()
            .get_node_info(req)
            .await
            .map(|resp| resp.into_inner().node)
    }
}

#[async_trait]
impl MessageSigner for Client {
    async fn derive_key(&mut self, key_loc: KeyLocator) -> Result<Vec<u8>, Status> {
        match self.wallet().derive_key(key_loc).await {
            Ok(resp) => Ok(resp.into_inner().raw_key_bytes),
            Err(e) => Err(e),
        }
    }

    async fn sign_message(
        &mut self,
        key_loc: KeyLocator,
        merkle_root: Hash,
        tag: String,
    ) -> Result<Vec<u8>, Status> {
        let tag_vec = tag.as_bytes().to_vec();
        let req = SignMessageReq {
            msg: <bitcoin::hashes::sha256::Hash as AsRef<[u8; 32]>>::as_ref(&merkle_root).to_vec(),
            tag: tag_vec,
            key_loc: Some(key_loc),
            schnorr_sig: true,
            ..Default::default()
        };

        let resp = self.signer().sign_message(req).await?;

        let resp_inner = resp.into_inner();
        Ok(resp_inner.signature)
    }

    fn sign_uir(
        &mut self,
        key_loc: KeyLocator,
        unsigned_invoice_req: UnsignedInvoiceRequest,
    ) -> Result<InvoiceRequest, OfferError> {
        let signer = LndkSigner {
            client: self.clone(),
            key_loc,
        };
        match unsigned_invoice_req.sign(signer) {
            Ok(signed_invoice) => Ok(signed_invoice),
            Err(_) => Err(OfferError::SignError(SignError::Signing)),
        }
    }
}

struct LndkSigner {
    client: Client,
    key_loc: KeyLocator,
}

impl SignInvoiceRequestFn for LndkSigner {
    fn sign_invoice_request(&self, msg: &UnsignedInvoiceRequest) -> Result<Signature, ()> {
        let tagged_hash = msg.as_ref();
        let tag = tagged_hash.tag().to_string();

        let mut signer = self.client.clone();
        let signature = match block_on(signer.sign_message(
            self.key_loc.clone(),
            tagged_hash.merkle_root(),
            tag,
        )) {
            Ok(sig) => sig,
            Err(_e) => return Err(()),
        };

        let ret = match Signature::from_slice(&signature) {
            Ok(s) => s,
            Err(_) => return Err(()),
        };
        Ok(ret)
    }
}

#[async_trait]
impl InvoicePayer for Client {
    async fn query_routes(
        &mut self,
        path: BlindedPath,
        cltv_expiry_delta: u16,
        fee_base_msat: u32,
        fee_ppm: u32,
        msats: u64,
    ) -> Result<QueryRoutesResponse, Status> {
        let mut blinded_hops = vec![];
        for hop in path.blinded_hops.iter() {
            let new_hop = tonic_lnd::lnrpc::BlindedHop {
                blinded_node: hop.blinded_node_id.serialize().to_vec(),
                encrypted_data: hop.clone().encrypted_payload,
            };
            blinded_hops.push(new_hop);
        }

        let introduction_node = match path.introduction_node {
            IntroductionNode::NodeId(pubkey) => pubkey,
            IntroductionNode::DirectedShortChannelId(direction, scid) => {
                match get_node_id(self.clone(), scid, direction).await {
                    Ok(pubkey) => pubkey,
                    Err(e) => {
                        error!("{e}");
                        return Err(Status::unknown("Could not get node id."));
                    }
                }
            }
        };
        let blinded_path = Some(tonic_lnd::lnrpc::BlindedPath {
            introduction_node: introduction_node.serialize().to_vec(),
            blinding_point: path.blinding_point.serialize().to_vec(),
            blinded_hops,
        });

        let blinded_payment_paths = tonic_lnd::lnrpc::BlindedPaymentPath {
            blinded_path,
            total_cltv_delta: u32::from(cltv_expiry_delta) + 120,
            base_fee_msat: u64::from(fee_base_msat),
            proportional_fee_msat: u64::from(fee_ppm),
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
    let get_info_request = ChanInfoRequest { chan_id: scid };
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
    use bitcoin::secp256k1::{KeyPair, Secp256k1, SecretKey};
    use lightning::offers::merkle::SignError;
    use lightning::offers::offer::{OfferBuilder, Quantity};
    use mockall::mock;
    use std::collections::HashMap;
    use std::str::FromStr;
    use std::time::{Duration, SystemTime};
    use tonic_lnd::lnrpc::{NodeAddress, Payment};

    fn get_offer() -> String {
        "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzrcgqgn3qzsyvfkx26qkyypvr5hfx60h9w9k934lt8s2n6zc0wwtgqlulw7dythr83dqx8tzumg".to_string()
    }

    fn build_custom_offer(amount_msats: u64) -> Offer {
        let secp_ctx = Secp256k1::new();
        let keys = KeyPair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap());
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

    fn get_pubkey() -> String {
        "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba".to_string()
    }

    fn get_invoice_request(offer: Offer, amount: u64) -> InvoiceRequest {
        let secp_ctx = Secp256k1::new();
        let keys = KeyPair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap());
        let pubkey = PublicKey::from(keys);
        offer
            .request_invoice(vec![42; 64], pubkey)
            .unwrap()
            .chain(Network::Regtest)
            .unwrap()
            .amount_msats(amount)
            .unwrap()
            .build()
            .unwrap()
            .sign(|message: &UnsignedInvoiceRequest| {
                Ok(secp_ctx.sign_schnorr_no_aux_rand(message.as_ref().as_digest(), &keys))
            })
            .expect("failed verifying signature")
    }

    fn get_blinded_path() -> BlindedPath {
        let entropy_source = MessengerUtilities::new();
        let secp_ctx = Secp256k1::new();
        BlindedPath::new_for_message(
            &[PublicKey::from_str(&get_pubkey()).unwrap()],
            &entropy_source,
            &secp_ctx,
        )
        .unwrap()
    }

    mock! {
        TestBolt12Signer{}

         #[async_trait]
         impl MessageSigner for TestBolt12Signer {
             async fn derive_key(&mut self, key_loc: KeyLocator) -> Result<Vec<u8>, Status>;
             async fn sign_message(&mut self, key_loc: KeyLocator, merkle_hash: Hash, tag: String) -> Result<Vec<u8>, Status>;
             fn sign_uir(&mut self, key_loc: KeyLocator, unsigned_invoice_req: UnsignedInvoiceRequest) -> Result<InvoiceRequest, OfferError>;
         }
    }

    mock! {
        TestPeerConnector{}

         #[async_trait]
         impl PeerConnector for TestPeerConnector {
             async fn list_peers(&mut self) -> Result<ListPeersResponse, Status>;
             async fn get_node_info(&mut self, pub_key: String) -> Result<Option<LightningNode>, Status>;
             async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
         }
    }

    mock! {
    TestInvoicePayer{}

        #[async_trait]
        impl InvoicePayer for TestInvoicePayer{
            async fn query_routes(&mut self, path: BlindedPath, cltv_expiry_delta: u16, fee_base_msat: u32, fee_ppm: u32, msats: u64) -> Result<QueryRoutesResponse, Status>;
            async fn send_to_route(&mut self, payment_hash: [u8; 32], route: Route) -> Result<HtlcAttempt, Status>;
            async fn track_payment(&mut self, payment_hash: [u8; 32]) -> Result<Payment, OfferError>;
        }
    }

    #[tokio::test]
    async fn test_request_invoice() {
        let mut signer_mock = MockTestBolt12Signer::new();

        signer_mock.expect_derive_key().returning(|_| {
            let pubkey = PublicKey::from_str(&get_pubkey()).unwrap();
            Ok(pubkey.serialize().to_vec())
        });

        let offer = decode(get_offer()).unwrap();
        let offer_amount = offer.amount().unwrap();
        let amount = match offer_amount {
            Amount::Bitcoin { amount_msats } => *amount_msats,
            _ => panic!("unexpected amount type"),
        };

        signer_mock
            .expect_sign_uir()
            .returning(move |_, _| Ok(get_invoice_request(offer.clone(), amount)));

        let offer = decode(get_offer()).unwrap();
        let handler = OfferHandler::new();
        let resp = handler
            .create_invoice_request(signer_mock, offer, vec![], Network::Regtest, Some(amount))
            .await;
        assert!(resp.is_ok())
    }

    #[tokio::test]
    async fn test_request_invoice_derive_key_error() {
        let mut signer_mock = MockTestBolt12Signer::new();

        signer_mock
            .expect_derive_key()
            .returning(|_| Err(Status::unknown("error testing")));

        signer_mock
            .expect_sign_uir()
            .returning(move |_, _| Ok(get_invoice_request(decode(get_offer()).unwrap(), 10000)));

        let offer = decode(get_offer()).unwrap();
        let handler = OfferHandler::new();
        assert!(handler
            .create_invoice_request(signer_mock, offer, vec![], Network::Regtest, Some(10000))
            .await
            .is_err())
    }

    #[tokio::test]
    async fn test_request_invoice_signer_error() {
        let mut signer_mock = MockTestBolt12Signer::new();

        signer_mock.expect_derive_key().returning(|_| {
            Ok(PublicKey::from_str(&get_pubkey())
                .unwrap()
                .serialize()
                .to_vec())
        });

        signer_mock
            .expect_sign_uir()
            .returning(move |_, _| Err(OfferError::SignError(SignError::Signing)));

        let offer = decode(get_offer()).unwrap();
        let handler = OfferHandler::new();
        assert!(handler
            .create_invoice_request(signer_mock, offer, vec![], Network::Regtest, Some(10000))
            .await
            .is_err())
    }

    #[tokio::test]
    async fn test_validate_amount() {
        // If the amount the user provided is greater than the offer-provided amount, then
        // we should be good.
        let offer = build_custom_offer(20000);
        assert!(validate_amount(offer.amount(), Some(20000)).await.is_ok());

        let offer = build_custom_offer(0);
        assert!(validate_amount(offer.amount(), Some(20000)).await.is_ok());
    }

    #[tokio::test]
    async fn test_validate_invalid_amount() {
        // If the amount the user provided is lower than the offer amount, we error.
        let offer = build_custom_offer(20000);
        assert!(validate_amount(offer.amount(), Some(1000)).await.is_err());

        // Both user amount and offer amount can't be 0.
        let offer = build_custom_offer(0);
        assert!(validate_amount(offer.amount(), None).await.is_err());
    }

    #[tokio::test]
    async fn test_connect_peer() {
        let mut connector_mock = MockTestPeerConnector::new();
        connector_mock.expect_list_peers().returning(|| {
            Ok(ListPeersResponse {
                ..Default::default()
            })
        });

        connector_mock.expect_get_node_info().returning(|_| {
            let node_addr = NodeAddress {
                network: String::from("regtest"),
                addr: String::from("127.0.0.1"),
            };
            let node = LightningNode {
                addresses: vec![node_addr],
                ..Default::default()
            };

            Ok(Some(node))
        });

        connector_mock
            .expect_connect_peer()
            .returning(|_, _| Ok(()));

        let pubkey = PublicKey::from_str(&get_pubkey()).unwrap();
        assert!(connect_to_peer(connector_mock, pubkey).await.is_ok());
    }

    #[tokio::test]
    async fn test_connect_peer_already_connected() {
        let mut connector_mock = MockTestPeerConnector::new();
        connector_mock.expect_list_peers().returning(|| {
            let peer = tonic_lnd::lnrpc::Peer {
                pub_key: get_pubkey(),
                ..Default::default()
            };

            Ok(ListPeersResponse {
                peers: vec![peer],
                ..Default::default()
            })
        });

        connector_mock.expect_get_node_info().returning(|_| {
            let node_addr = NodeAddress {
                network: String::from("regtest"),
                addr: String::from("127.0.0.1"),
            };
            let node = LightningNode {
                addresses: vec![node_addr],
                ..Default::default()
            };

            Ok(Some(node))
        });

        let pubkey = PublicKey::from_str(&get_pubkey()).unwrap();
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

        connector_mock.expect_get_node_info().returning(|_| {
            let node_addr = NodeAddress {
                network: String::from("regtest"),
                addr: String::from("127.0.0.1"),
            };
            let node = LightningNode {
                addresses: vec![node_addr],
                ..Default::default()
            };

            Ok(Some(node))
        });

        connector_mock
            .expect_connect_peer()
            .returning(|_, _| Err(Status::unknown("")));

        let pubkey = PublicKey::from_str(&get_pubkey()).unwrap();
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
                pub_key: get_pubkey(),
                features: feature_entry,
                ..Default::default()
            };
            Ok(ListPeersResponse { peers: vec![peer] })
        });

        let receiver_node_id = PublicKey::from_str(&get_pubkey()).unwrap();
        let handler = OfferHandler::new();
        assert!(handler
            .create_reply_path(connector_mock, receiver_node_id)
            .await
            .is_ok())
    }

    #[tokio::test]
    // Test that create_reply_path works fine when no suitable introduction node peer is found.
    async fn test_create_reply_path_no_intro_node() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_peers()
            .returning(|| Ok(ListPeersResponse { peers: vec![] }));

        let receiver_node_id = PublicKey::from_str(&get_pubkey()).unwrap();
        let handler = OfferHandler::new();
        assert!(handler
            .create_reply_path(connector_mock, receiver_node_id)
            .await
            .is_ok())
    }

    #[tokio::test]
    async fn test_create_reply_path_list_peers_error() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_peers()
            .returning(|| Err(Status::unknown("unknown error")));

        let receiver_node_id = PublicKey::from_str(&get_pubkey()).unwrap();
        let handler = OfferHandler::new();
        assert!(handler
            .create_reply_path(connector_mock, receiver_node_id)
            .await
            .is_err())
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

        let blinded_path = get_blinded_path();
        let payment_hash = MessengerUtilities::new().get_secure_random_bytes();
        let handler = OfferHandler::new();
        let payment_id = PaymentId(MessengerUtilities::new().get_secure_random_bytes());
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

        let blinded_path = get_blinded_path();
        let payment_hash = MessengerUtilities::new().get_secure_random_bytes();
        let payment_id = PaymentId(MessengerUtilities::new().get_secure_random_bytes());
        let handler = OfferHandler::new();
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

        let blinded_path = get_blinded_path();
        let payment_hash = MessengerUtilities::new().get_secure_random_bytes();
        let payment_id = PaymentId(MessengerUtilities::new().get_secure_random_bytes());
        let handler = OfferHandler::new();
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
}
