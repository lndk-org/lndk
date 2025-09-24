use std::{
    str::FromStr,
    time::{Duration, SystemTime},
};

use bitcoin::{hashes::Hash, key::Secp256k1, secp256k1::PublicKey, Network};
use lightning::{
    blinded_path::{
        message::{BlindedMessagePath, MessageContext, OffersContext},
        payment::BlindedPaymentPath,
        Direction, IntroductionNode,
    },
    ln::{
        channelmanager::{PaymentId, Verification},
        inbound_payment::ExpandedKey,
    },
    offers::{
        invoice_request::InvoiceRequest,
        nonce::Nonce,
        offer::{Offer, OfferBuilder, Quantity},
    },
    onion_message::{
        messenger::{Destination, MessageSendInstructions},
        offers::OffersMessage,
    },
    sign::EntropySource,
    types::payment::PaymentHash,
};
use log::{debug, error, info, trace};
use tokio::{select, time::sleep};
use tonic_lnd::{
    lnrpc::{ChanInfoRequest, GetInfoRequest, Payment},
    Client,
};
use triggered::Listener;

use crate::{
    lnd::{
        features_support_onion_messages, parse_blinded_paths, Bolt12InvoiceCreator, InvoicePayer,
        OfferCreator, PeerConnector,
    },
    offers::handler::{CreateOfferParams, SendPaymentParams},
    onion_messenger::MessengerUtilities,
};
use std::collections::HashSet;

use super::{validate_amount, OfferError};

const INITIAL_DELAY_MS: u64 = 500;
const MAX_DELAY_MS: u64 = 60_000;
const MAX_ATTEMPTS: u32 = 10;

#[derive(Debug)]
pub struct LndkBolt12InvoiceInfo {
    pub payment_hash: PaymentHash,
    pub payment_paths: Vec<BlindedPaymentPath>,
}

pub(super) async fn create_invoice_request(
    offer: Offer,
    network: Network,
    entropy_source: &MessengerUtilities,
    expanded_key: ExpandedKey,
    msats: Option<u64>,
    payer_note: Option<String>,
) -> Result<(InvoiceRequest, PaymentId, u64, OffersContext), OfferError> {
    let validated_amount = validate_amount(offer.amount().as_ref(), msats).await?;

    let payment_id = PaymentId(entropy_source.get_secure_random_bytes());

    // We need to add some metadata to the invoice request to help with verification of the
    // invoice once returned from the offer maker. Once we get an invoice back, this metadata
    // will help us to determine: 1) That the invoice is truly for the invoice request we sent.
    // 2) We don't pay duplicate invoices.
    let secp_ctx = Secp256k1::new();
    let nonce = lightning::offers::nonce::Nonce::from_entropy_source(entropy_source);
    let builder = offer
        .request_invoice(&expanded_key, nonce, &secp_ctx, payment_id)
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

    let hmac = payment_id.hmac_for_offer_payment(nonce, &expanded_key);
    let offer_context = OffersContext::OutboundPayment {
        payment_id,
        nonce,
        hmac: Some(hmac),
    };
    Ok((invoice_request, payment_id, validated_amount, offer_context))
}

/// Sends a payment using the provided payer client and payment parameters.
///
/// This function performs a two-step payment process:
/// 1. Queries available routes using the payment parameters (destination, amount, fees, etc.)
/// 2. Sends the payment using the first available route
///
/// The function will return an error if either route querying or payment sending fails.
/// On success, it returns `Ok(())` indicating the payment was successfully dispatched.
pub(crate) async fn send_payment(
    mut payer: impl InvoicePayer + std::marker::Send + 'static,
    params: SendPaymentParams,
) -> Result<(), OfferError> {
    let resp = payer
        .query_routes(
            params.path,
            params.cltv_expiry_delta,
            params.fee_base_msat,
            params.fee_ppm,
            params.msats,
            params.fee_limit,
        )
        .await
        .map_err(|e| OfferError::RouteFailure(e.message().to_string()))?;

    trace!("Routes found {}...", resp.routes.len());
    let resp = payer
        .send_to_route(params.payment_hash, resp.routes[0].clone())
        .await
        .map_err(|e| OfferError::RouteFailure(e.message().to_string()))?;

    trace!(
        "Sent payment using preimage {} using attempt_id {} with status {}. {}",
        hex::encode(resp.preimage),
        resp.attempt_id,
        resp.status,
        resp.failure.map_or("".to_string(), |f| format!(
            "failed with reason: {}.",
            f.code
        ))
    );
    Ok(())
}

pub(super) async fn track_payment(
    mut payer: impl InvoicePayer + std::marker::Send + 'static,
    payment_hash: [u8; 32],
) -> Result<Payment, OfferError> {
    payer
        .track_payment(payment_hash)
        .await
        .map_err(|_| OfferError::PaymentFailure)
}

pub(super) struct CreateOfferArgs {
    amount_msats: u64,
    chain: Network,
    description: Option<String>,
    issuer: Option<String>,
    quantity: Option<Quantity>,
    expiry: Option<Duration>,
}

impl CreateOfferArgs {
    pub fn from_params(params: &CreateOfferParams) -> Self {
        Self {
            amount_msats: params.amount_msats,
            chain: params.chain,
            description: params.description.clone(),
            issuer: params.issuer.clone(),
            quantity: params.quantity,
            expiry: params.expiry,
        }
    }
}

pub(super) async fn create_offer(
    mut creator: (impl OfferCreator + std::marker::Send + 'static + PeerConnector),
    args: CreateOfferArgs,
    entropy_source: &MessengerUtilities,
    expanded_key: &ExpandedKey,
) -> Result<Offer, OfferError> {
    let info = creator
        .get_info()
        .await
        .map_err(|_| OfferError::NodeAddressNotFound)?;
    let node_id =
        PublicKey::from_str(&info.identity_pubkey).map_err(|_| OfferError::NodeAddressNotFound)?;
    let nonce = Nonce::from_entropy_source(entropy_source);
    let secp_ctx = Secp256k1::new();

    let message_context = MessageContext::Offers(OffersContext::InvoiceRequest { nonce });
    let paths =
        create_reply_path_for_offer_creation(creator, node_id, message_context, entropy_source)
            .await?;

    let mut builder =
        OfferBuilder::deriving_signing_pubkey(node_id, expanded_key, nonce, &secp_ctx)
            .amount_msats(args.amount_msats)
            .chain(args.chain);

    builder = if let Some(path) = paths.first() {
        builder.path(path.clone())
    } else {
        builder
    };

    builder = if let Some(path) = paths.get(1) {
        builder.path(path.clone())
    } else {
        builder
    };

    builder = if let Some(path) = paths.get(2) {
        builder.path(path.clone())
    } else {
        builder
    };

    builder = match args.description {
        Some(description) => builder.description(description),
        None => builder,
    };
    builder = match args.issuer {
        Some(issuer) => builder.issuer(issuer),
        None => builder,
    };
    builder = match args.quantity {
        Some(quantity) => builder.supported_quantity(quantity),
        None => builder,
    };
    builder = match args.expiry {
        Some(expiry) => {
            let expiry = SystemTime::now() + expiry;
            let absolute_expiry = expiry
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|_| OfferError::CreateOfferTimeFailure)?;
            builder.absolute_expiry(absolute_expiry)
        }
        None => builder,
    };

    let offer = builder.build().map_err(OfferError::CreateOfferFailure)?;
    Ok(offer)
}

pub(super) async fn create_invoice_info_from_request(
    mut creator: impl Bolt12InvoiceCreator + std::marker::Send + 'static,
    invoice_request: InvoiceRequest,
) -> Result<LndkBolt12InvoiceInfo, OfferError> {
    log::trace!("Creating invoice");
    let invoice_response = creator
        .add_invoice(invoice_request)
        .await
        .map_err(|e| OfferError::AddInvoiceFailure(e.message().to_string()))?;
    let payment_request = invoice_response.payment_request;
    log::trace!("Payment request: {:?}", payment_request);
    let payreq = creator
        .decode_payment_request(payment_request)
        .await
        .map_err(|e| OfferError::DecodePaymentRequestFailure(e.message().to_string()))?;
    log::trace!("Decoded payment request: {:?}", payreq);
    let payment_hash =
        bitcoin::hashes::sha256::Hash::from_str(&payreq.payment_hash).map_err(|e| {
            error!("Could not parse payment hash. {e}");
            OfferError::ParsePaymentHashFailure(e.to_string())
        })?;

    let payment_hash = PaymentHash(*payment_hash.as_byte_array());

    let payment_paths = parse_blinded_paths(payreq.blinded_paths);
    Ok(LndkBolt12InvoiceInfo {
        payment_hash,
        payment_paths,
    })
}

/// create_reply_path_for_offer_creation creates blinded paths to provide to the offer.
/// We try to find at least one active public channel and at most three active public channels that
/// can act as blinded paths.
///
/// Otherwise we create a blinded path directly to ourselves.
pub async fn create_reply_path_for_offer_creation(
    mut connector: impl PeerConnector + std::marker::Send + 'static,
    node_id: PublicKey,
    message_context: MessageContext,
    messenger_utils: &MessengerUtilities,
) -> Result<Vec<BlindedMessagePath>, OfferError> {
    // Find introduction channels for our blinded paths.
    let current_channels = connector.list_active_public_channels().await.map_err(|e| {
        error!("Could not lookup current channels: {e}.");
        OfferError::ListChannelsFailure(e.message().to_string())
    })?;

    let mut intro_channels = HashSet::new();
    for channel in current_channels.channels.iter() {
        let pubkey = channel.remote_pubkey.clone();
        if intro_channels.len() > 3 {
            break;
        }
        match connector.get_node_info(pubkey, true).await {
            Ok(node_info) => match node_info.node {
                Some(node) => {
                    if node_info.channels.is_empty() {
                        continue;
                    }
                    let onion_support = features_support_onion_messages(&node.features);
                    let other_channels = node_info
                        .channels
                        .into_iter()
                        .filter(|peer_channel| peer_channel.channel_id != channel.chan_id)
                        .collect::<Vec<tonic_lnd::lnrpc::ChannelEdge>>();
                    if !other_channels.is_empty() && onion_support {
                        let pubkey = PublicKey::from_str(&channel.remote_pubkey).unwrap();
                        intro_channels.insert(pubkey);
                    }
                }
                None => continue,
            },
            Err(_) => continue,
        }
    }

    let secp_ctx = Secp256k1::new();
    if intro_channels.is_empty() {
        debug!(
            "Failed to create a blinded path for the offer. 
            No active public channels were found on which to build a path."
        );
        let path =
            BlindedMessagePath::one_hop(node_id, message_context, messenger_utils, &secp_ctx)
                .map_err(|_| {
                    error!("Could not create blinded path.");
                    OfferError::BuildBlindedPathFailure
                })?;
        Ok(vec![path])
    } else {
        let mut paths = vec![];
        for node in intro_channels {
            let nodes = vec![lightning::blinded_path::message::MessageForwardNode {
                node_id: node,
                short_channel_id: None,
            }];
            let path = BlindedMessagePath::new(
                &nodes,
                node_id,
                message_context.clone(),
                messenger_utils,
                &secp_ctx,
            )
            .map_err(|_| {
                error!("Could not create blinded path.");
                OfferError::BuildBlindedPathFailure
            })?;
            paths.push(path)
        }
        Ok(paths)
    }
}

/// create_reply_path_for_outgoing_payments creates a blinded path to provide to the offer node when
/// requesting an invoice so they know where to send the invoice back to. We try to find a peer that
/// we're connected to with the necessary requirements to form a blinded path. The peer needs two
/// things:
/// 1) Onion messaging support.
/// 2) To be an advertised node with at least one public channel.
///
/// Otherwise we create a blinded path directly to ourselves.
pub async fn create_reply_path_for_outgoing_payments(
    mut connector: impl PeerConnector + std::marker::Send + 'static,
    node_id: PublicKey,
    message_context: MessageContext,
    messenger_utils: &MessengerUtilities,
) -> Result<BlindedMessagePath, OfferError> {
    // Find an introduction node for our blinded path.
    let current_peers = connector.list_peers().await.map_err(|e| {
        error!("Could not lookup current peers: {e}.");
        OfferError::ListPeersFailure(e.message().to_string())
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
        Ok(
            BlindedMessagePath::one_hop(node_id, message_context, messenger_utils, &secp_ctx)
                .map_err(|_| {
                    error!("Could not create blinded path.");
                    OfferError::BuildBlindedPathFailure
                })?,
        )
    } else {
        let nodes = vec![lightning::blinded_path::message::MessageForwardNode {
            node_id: intro_node.unwrap(),
            short_channel_id: None,
        }];
        Ok(
            BlindedMessagePath::new(&nodes, node_id, message_context, messenger_utils, &secp_ctx)
                .map_err(|_| {
                error!("Could not create blinded path.");
                OfferError::BuildBlindedPathFailure
            })?,
        )
    }
}

pub async fn send_invoice_request(
    destination: Destination,
    mut client: Client,
    invoice_request: InvoiceRequest,
    offer_context: OffersContext,
    messenger_utils: &MessengerUtilities,
) -> Result<(OffersMessage, MessageSendInstructions), OfferError> {
    // For now we connect directly to the introduction node of the blinded path so we don't need
    // any intermediate nodes here. In the future we'll query for a full path to the
    // introduction node for better sender privacy.
    match destination {
        Destination::Node(pubkey) => connect_to_peer(client.lightning().clone(), pubkey).await?,
        Destination::BlindedPath(ref path) => match path.introduction_node() {
            IntroductionNode::NodeId(pubkey) => {
                connect_to_peer(client.lightning().clone(), *pubkey).await?
            }
            IntroductionNode::DirectedShortChannelId(direction, scid) => {
                let pubkey = get_node_id(client.clone(), *scid, *direction).await?;
                connect_to_peer(client.lightning().clone(), pubkey).await?
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
    let reply_path = create_reply_path_for_outgoing_payments(
        client.lightning().clone(),
        pubkey,
        message_context,
        messenger_utils,
    )
    .await?;

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

    let contents = OffersMessage::InvoiceRequest(invoice_request);
    trace!("Sending invoice request with reply path");

    let send_instructions = MessageSendInstructions::WithSpecifiedReplyPath {
        destination,
        reply_path,
    };

    Ok((contents, send_instructions))
}

pub(crate) async fn connect_to_peer_with_retry(
    connector: impl PeerConnector + Clone,
    node_id: PublicKey,
    shutdown_listener: Listener,
) -> Result<(), OfferError> {
    let mut attempts = 0;
    loop {
        select! {
            biased;
            _ = shutdown_listener.clone() => {
                info!("Received shutdown signal, exiting connect to peer loop.");
                return Ok(())
            }
            _ = async {
                if attempts > 0 {
                    let mut delay = INITIAL_DELAY_MS * (2u64.pow(attempts - 1));
                    delay = delay.min(MAX_DELAY_MS);

                    info!("Attempt {attempts} to connect to peer failed. Retrying in {}ms...", delay);
                    sleep(Duration::from_millis(delay)).await;
                }
            } => {
                match connect_to_peer(connector.clone(), node_id).await {
                    Ok(_) => {
                        debug!("Connect to peer with node_id {node_id} successful.");
                        return Ok(())
                    },
                    Err(e) => {
                        error!("Connect to peer produced an error: {e}.");
                        attempts += 1;
                        if attempts > MAX_ATTEMPTS {
                            error!("Max retry attempts reached for peer {node_id}. Giving up.");
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
}

pub(crate) async fn connect_to_peer(
    mut connector: impl PeerConnector,
    node_id: PublicKey,
) -> Result<(), OfferError> {
    let resp = connector
        .list_peers()
        .await
        .map_err(|e| OfferError::PeerConnectError(e.message().to_string()))?;

    let node_id_str = node_id.to_string();
    for peer in resp.peers.iter() {
        if peer.pub_key == node_id_str {
            log::trace!(
                "Peer already known while connecting as introduction node: {}",
                node_id_str
            );
            return Ok(());
        }
    }

    let node = connector
        .get_node_info(node_id_str.clone(), false)
        .await
        .map_err(|e| OfferError::PeerConnectError(e.message().to_string()))?;

    let node = match node.node {
        Some(node) => node,
        None => return Err(OfferError::NodeAddressNotFound),
    };

    if node.addresses.is_empty() {
        return Err(OfferError::NodeAddressNotFound);
    }

    connector
        .connect_peer(node_id_str, node.addresses[0].clone().addr)
        .await
        .map_err(|e| OfferError::PeerConnectError(e.message().to_string()))?;

    Ok(())
}

// get_node_id finds an introduction node from the scid and direction provided by a blinded path.
pub(crate) async fn get_node_id(
    client: Client,
    scid: u64,
    direction: Direction,
) -> Result<PublicKey, OfferError> {
    let pubkey = get_node_id_from_scid(client, scid, direction).await?;
    PublicKey::from_slice(pubkey.as_bytes()).map_err(|e| {
        error!("Could not parse pubkey. {e}");
        OfferError::IntroductionNodeNotFound
    })
}

pub(super) async fn get_node_id_from_scid(
    client: Client,
    scid: u64,
    direction: Direction,
) -> Result<String, OfferError> {
    let get_info_request = ChanInfoRequest {
        chan_id: scid,
        chan_point: "".to_string(),
    };
    let channel_info = client
        .lightning_read_only()
        .get_chan_info(get_info_request)
        .await
        .map_err(|e| OfferError::GetChannelInfo(e.message().to_string()))?
        .into_inner();
    match direction {
        Direction::NodeOne => Ok(channel_info.node1_pub),
        Direction::NodeTwo => Ok(channel_info.node2_pub),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::offers::client_impls::tests::{
        MockTestBolt12InvoiceCreator, MockTestInvoicePayer, MockTestOfferCreator,
        MockTestPeerConnector,
    };
    use crate::offers::decode;
    use lightning::{
        blinded_path::payment::{
            BlindedPaymentPath, Bolt12OfferContext, ForwardTlvs, PaymentConstraints,
            PaymentContext, PaymentForwardNode, PaymentRelay, UnauthenticatedReceiveTlvs,
        },
        bolt11_invoice::PaymentSecret,
        offers::{
            invoice_request::InvoiceRequestFields,
            nonce::Nonce,
            offer::{Amount, OfferId},
        },
        types::features::BlindedHopFeatures,
        util::string::UntrustedString,
    };
    use mockall::predicate::eq;
    use tonic_lnd::{
        lnrpc::{
            AddInvoiceResponse, ChannelEdge, GetInfoResponse, HtlcAttempt, LightningNode,
            ListChannelsResponse, ListPeersResponse, NodeAddress, NodeInfo, PayReq,
            QueryRoutesResponse, Route,
        },
        tonic::Status,
    };
    const NONCE_BYTES: &[u8] = &[42u8; 16];

    fn get_offer() -> String {
        "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzrcgqgn3qzsyvfkx26qkyypvr5hfx60h9w9k934lt8s2n6zc0wwtgqlulw7dythr83dqx8tzumg".to_string()
    }

    fn get_pubkeys() -> Vec<String> {
        let pubkey1 =
            "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba".to_string();
        let pubkey2 =
            "03b060a3b572ab060532fbe49506fe25b5957195733788aab01ab3c0f40bb52602".to_string();
        vec![pubkey1, pubkey2]
    }

    fn get_message_context() -> MessageContext {
        let offer_context = OffersContext::OutboundPayment {
            payment_id: PaymentId([42; 32]),
            nonce: Nonce::try_from(NONCE_BYTES).unwrap(),
            hmac: None,
        };
        MessageContext::Offers(offer_context)
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
        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);
        let resp = create_invoice_request(
            offer,
            Network::Regtest,
            &entropy_source,
            expanded_key,
            Some(amount),
            Some("".to_string()),
        )
        .await;
        assert!(resp.is_ok())
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

            Ok(ListPeersResponse { peers: vec![peer] })
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
    // Test that create_reply_path works fine when no suitable introduction node peer is found.
    async fn test_create_reply_path_no_intro_node() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_peers()
            .returning(|| Ok(ListPeersResponse { peers: vec![] }));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let response = create_reply_path_for_outgoing_payments(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());

        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_active_public_channels()
            .returning(|| Ok(ListChannelsResponse { channels: vec![] }));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let response = create_reply_path_for_offer_creation(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
    }

    #[tokio::test]
    async fn test_reply_path_for_outgoing_payments() {
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
        let response = create_reply_path_for_outgoing_payments(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
    }

    #[tokio::test]
    async fn test_create_reply_path_for_offer_creation() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_active_public_channels()
            .returning(|| {
                let channel = tonic_lnd::lnrpc::Channel {
                    remote_pubkey: get_pubkeys()[0].clone(),
                    ..Default::default()
                };
                Ok(ListChannelsResponse {
                    channels: vec![channel],
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

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();

        let message_context = get_message_context();
        let response = create_reply_path_for_offer_creation(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
    }

    #[tokio::test]
    async fn test_create_reply_path_list_peers_error() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_peers()
            .returning(|| Err(Status::unknown("unknown error")));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let response = create_reply_path_for_outgoing_payments(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_err());

        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_active_public_channels()
            .returning(|| Err(Status::unknown("unknown error")));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let response = create_reply_path_for_offer_creation(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_err());
    }

    #[tokio::test]
    async fn test_create_reply_path_for_outgoing_payments_not_advertised() {
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
        let response = create_reply_path_for_outgoing_payments(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
        let reply_path = response.unwrap();
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
        let message_context = get_message_context();
        let response = create_reply_path_for_outgoing_payments(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
        let reply_path = response.unwrap();
        let hops = reply_path.blinded_hops();
        assert!(hops.len() == 2);
    }

    #[tokio::test]
    async fn test_create_reply_path_for_offer_creation_not_advertised() {
        // First lets test that the one that we are trying to connect to, does not have public
        // channels This should return a blinded path with only one hop.
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_active_public_channels()
            .returning(|| Ok(ListChannelsResponse { channels: vec![] }));

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

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let response = create_reply_path_for_offer_creation(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
        let reply_path = response.unwrap();
        let hops = reply_path.first().unwrap().blinded_hops();
        assert!(hops.len() == 1);

        // Now let's test that we are connected to two channels.
        // One channel isn't advertised (i.e. it has no public channels). But the second is. This
        // should succeed.
        let mut connector_mock = MockTestPeerConnector::new();

        // Only second channel is advertised.
        connector_mock
            .expect_list_active_public_channels()
            .returning(|| {
                let keys = get_pubkeys();

                let channel2 = tonic_lnd::lnrpc::Channel {
                    remote_pubkey: keys[1].clone(),
                    chan_id: 10,
                    ..Default::default()
                };

                Ok(ListChannelsResponse {
                    channels: vec![channel2],
                })
            });

        connector_mock.expect_get_node_info().returning(|_, _| {
            let node_addr = NodeAddress {
                network: String::from("regtest"),
                addr: String::from("127.0.0.1"),
            };
            let mut features = std::collections::HashMap::new();
            // Add onion message feature (feature bit 38/39)
            features.insert(
                38,
                tonic_lnd::lnrpc::Feature {
                    name: "onion_messages_optional".to_string(),
                    is_required: false,
                    is_known: true,
                },
            );
            let node = Some(LightningNode {
                addresses: vec![node_addr],
                features,
                ..Default::default()
            });

            let node_info = NodeInfo {
                node,
                channels: vec![
                    ChannelEdge {
                        channel_id: 10,
                        ..Default::default()
                    },
                    ChannelEdge {
                        channel_id: 20,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            };

            Ok(node_info)
        });

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let response = create_reply_path_for_offer_creation(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
        let reply_path = response.unwrap();
        let hops = reply_path.first().unwrap().blinded_hops();
        assert!(hops.len() == 2);
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
            node_id,
            htlc_maximum_msat: 1_000_000_000_000,
        }];
        BlindedPaymentPath::new(
            &intermediate_nodes,
            node_id,
            payee_tlvs,
            u64::MAX,
            12344,
            &entropy_source,
            &secp_ctx,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_send_payment() {
        let mut payer_mock = MockTestInvoicePayer::new();

        payer_mock
            .expect_query_routes()
            .returning(|_, _, _, _, _, _| {
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
        let payment_id = PaymentId(MessengerUtilities::default().get_secure_random_bytes());
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash,
            msats: 2000,
            payment_id,
            fee_limit: None,
        };
        assert!(send_payment(payer_mock, params).await.is_ok());
    }

    #[tokio::test]
    async fn test_send_payment_query_error() {
        let mut payer_mock = MockTestInvoicePayer::new();

        payer_mock
            .expect_query_routes()
            .returning(|_, _, _, _, _, _| Err(Status::unknown("unknown error")));

        let blinded_path = get_blinded_payment_path();
        let payment_hash = MessengerUtilities::default().get_secure_random_bytes();
        let payment_id = PaymentId(MessengerUtilities::default().get_secure_random_bytes());
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash,
            msats: 2000,
            payment_id,
            fee_limit: None,
        };
        assert!(send_payment(payer_mock, params).await.is_err());
    }

    #[tokio::test]
    async fn test_send_payment_send_error() {
        let mut payer_mock = MockTestInvoicePayer::new();

        payer_mock
            .expect_query_routes()
            .returning(|_, _, _, _, _, _| {
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
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash,
            msats: 2000,
            payment_id,
            fee_limit: None,
        };
        assert!(send_payment(payer_mock, params).await.is_err());
    }

    #[tokio::test]
    // Test that a new key is created with each call to create_invoice_request. Transient keys
    // improve privacy and we also need them to successfully make multiple payments to the same CLN
    // offer.
    async fn test_transient_keys() {
        let offer = decode(get_offer()).unwrap();
        let offer_amount = offer.amount().unwrap();
        let amount = match offer_amount {
            Amount::Bitcoin { amount_msats } => amount_msats,
            _ => panic!("unexpected amount type"),
        };
        let offer = decode(get_offer()).unwrap();
        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);
        let resp_1 = create_invoice_request(
            offer.clone(),
            Network::Regtest,
            &entropy_source,
            expanded_key,
            Some(amount),
            Some("".to_string()),
        )
        .await;
        let resp_2 = create_invoice_request(
            offer,
            Network::Regtest,
            &entropy_source,
            expanded_key,
            Some(amount),
            Some("".to_string()),
        )
        .await;
        assert_ne!(
            resp_1.unwrap().0.payer_signing_pubkey(),
            resp_2.unwrap().0.payer_signing_pubkey()
        );
    }

    // Helper function to create a mock for successful create_offer tests
    fn setup_create_offer_success_mock() -> MockTestOfferCreator {
        let mut creator_mock = MockTestOfferCreator::new();

        // Mock get_info to return a valid response
        creator_mock.expect_get_info().returning(|| {
            Ok(GetInfoResponse {
                identity_pubkey: get_pubkeys()[1].clone(),
                ..Default::default()
            })
        });

        creator_mock.expect_list_peers().returning(|| {
            let peer = tonic_lnd::lnrpc::Peer {
                pub_key: get_pubkeys()[0].clone(),
                ..Default::default()
            };
            Ok(ListPeersResponse { peers: vec![peer] })
        });

        creator_mock
            .expect_list_active_public_channels()
            .returning(|| {
                let channel = tonic_lnd::lnrpc::Channel {
                    remote_pubkey: get_pubkeys()[0].clone(),
                    ..Default::default()
                };
                Ok(ListChannelsResponse {
                    channels: vec![channel],
                })
            });

        creator_mock.expect_get_node_info().returning(|_, _| {
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

        creator_mock
    }

    #[tokio::test]
    async fn test_create_offer_success() {
        let creator_mock = setup_create_offer_success_mock();

        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);

        let args = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: Some("Test offer".to_string()),
            issuer: Some("Test issuer".to_string()),
            quantity: Some(Quantity::One),
            expiry: None,
        };
        let result = create_offer(creator_mock, args, &entropy_source, &expanded_key).await;

        assert!(result.is_ok());
        let offer = result.unwrap();

        // Verify the offer properties
        assert_eq!(
            offer.amount().unwrap(),
            Amount::Bitcoin { amount_msats: 1000 }
        );
        assert_eq!(offer.description().unwrap().to_string(), "Test offer");
        assert_eq!(offer.issuer().unwrap().to_string(), "Test issuer");
        assert_eq!(offer.supported_quantity(), Quantity::One);
    }

    #[tokio::test]
    async fn test_create_offer_minimal() {
        let creator_mock = setup_create_offer_success_mock();

        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);

        // Only provide required parameters
        let args = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: None,
            issuer: None,
            quantity: None,
            expiry: None,
        };
        let result = create_offer(creator_mock, args, &entropy_source, &expanded_key).await;

        assert!(result.is_ok());
        let offer = result.unwrap();

        // Verify the offer has default values for optional parameters
        assert_eq!(
            offer.amount().unwrap(),
            Amount::Bitcoin { amount_msats: 1000 }
        );
        assert_eq!(offer.description().unwrap().to_string(), ""); // Empty description
        assert_eq!(offer.issuer(), None);
        assert_eq!(offer.supported_quantity(), Quantity::One);
    }

    #[tokio::test]
    async fn test_create_offer_with_expiry() {
        let creator_mock = setup_create_offer_success_mock();

        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);

        // Include expiry parameter
        let args = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: None,
            issuer: None,
            quantity: None,
            expiry: Some(Duration::from_secs(3600)),
        };
        let result = create_offer(creator_mock, args, &entropy_source, &expanded_key).await;

        assert!(result.is_ok());
        let offer = result.unwrap();

        // Verify the offer has an expiry set (not None)
        assert!(offer.absolute_expiry().is_some());
    }

    #[tokio::test]
    async fn test_create_offer_get_info_error() {
        let mut creator_mock = MockTestOfferCreator::new();

        // Mock get_info to return an error
        creator_mock
            .expect_get_info()
            .returning(|| Err(Status::internal("Failed to get node info")));

        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);

        let args = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: None,
            issuer: None,
            quantity: None,
            expiry: None,
        };
        let result = create_offer(creator_mock, args, &entropy_source, &expanded_key).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OfferError::NodeAddressNotFound
        ));
    }

    #[tokio::test]
    async fn test_create_offer_invalid_pubkey() {
        let mut creator_mock = MockTestOfferCreator::new();

        // Mock get_info to return an invalid pubkey
        creator_mock.expect_get_info().returning(|| {
            Ok(GetInfoResponse {
                identity_pubkey: "invalid_pubkey".to_string(),
                ..Default::default()
            })
        });

        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);

        let args = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: None,
            issuer: None,
            quantity: None,
            expiry: None,
        };
        let result = create_offer(creator_mock, args, &entropy_source, &expanded_key).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OfferError::NodeAddressNotFound
        ));
    }

    #[tokio::test]
    async fn test_create_offer_with_quantity() {
        let creator_mock = setup_create_offer_success_mock();

        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);

        // Test with different quantity types
        let args = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: None,
            issuer: None,
            quantity: Some(Quantity::Unbounded),
            expiry: None,
        };
        let result = create_offer(creator_mock, args, &entropy_source, &expanded_key).await;

        assert!(result.is_ok());
        let offer = result.unwrap();

        assert_eq!(offer.supported_quantity(), Quantity::Unbounded);
    }

    #[tokio::test]
    async fn test_create_offer_different_signing_pubkeys_and_ids() {
        let creator_mock1 = setup_create_offer_success_mock();
        let creator_mock2 = setup_create_offer_success_mock();

        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);

        let args = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: None,
            issuer: None,
            quantity: None,
            expiry: None,
        };

        let args2 = CreateOfferArgs {
            amount_msats: 1000,
            chain: Network::Regtest,
            description: None,
            issuer: None,
            quantity: None,
            expiry: None,
        };

        let offer1 = create_offer(creator_mock1, args, &entropy_source, &expanded_key).await;
        let offer2 = create_offer(creator_mock2, args2, &entropy_source, &expanded_key).await;

        assert!(offer1.is_ok());
        assert!(offer2.is_ok());
        let offer1 = offer1.unwrap();
        let offer2 = offer2.unwrap();

        assert_ne!(
            offer1.issuer_signing_pubkey().unwrap(),
            offer2.issuer_signing_pubkey().unwrap()
        );
        assert_ne!(offer1.id(), offer2.id());
    }

    // Helper function to create a dummy invoice request for testing
    async fn create_dummy_invoice_request() -> InvoiceRequest {
        let offer = decode(get_offer()).unwrap();
        let offer_amount = offer.amount().unwrap();
        let amount = match offer_amount {
            Amount::Bitcoin { amount_msats } => amount_msats,
            _ => panic!("unexpected amount type"),
        };
        let entropy_source = MessengerUtilities::new([42; 32]);
        let expanded_key = ExpandedKey::new([42; 32]);
        let (invoice_request, _, _, _) = create_invoice_request(
            offer,
            Network::Regtest,
            &entropy_source,
            expanded_key,
            Some(amount),
            None,
        )
        .await
        .unwrap();
        invoice_request
    }

    #[tokio::test]
    async fn test_create_invoice_info_from_request_success() {
        let mut creator_mock = MockTestBolt12InvoiceCreator::new();

        // Mock successful add_invoice
        creator_mock.expect_add_invoice().returning(|_| {
            Ok(AddInvoiceResponse {
                payment_request: "dummy_payment_request".to_string(),
                ..Default::default()
            })
        });

        // Mock successful decode_payment_request with valid 32-byte hex payment hash
        creator_mock.expect_decode_payment_request().returning(|_| {
            Ok(PayReq {
                payment_hash: "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
                    .to_string(),
                blinded_paths: vec![],
                ..Default::default()
            })
        });

        let invoice_request = create_dummy_invoice_request().await;
        let result = create_invoice_info_from_request(creator_mock, invoice_request).await;

        assert!(result.is_ok());
        let invoice_info = result.unwrap();
        assert_eq!(invoice_info.payment_hash.0.len(), 32);
        assert_eq!(invoice_info.payment_paths.len(), 0);
    }

    #[tokio::test]
    async fn test_create_invoice_info_from_request_add_invoice_failure() {
        let mut creator_mock = MockTestBolt12InvoiceCreator::new();

        // Mock add_invoice failure
        creator_mock
            .expect_add_invoice()
            .returning(|_| Err(Status::internal("Failed to add invoice")));

        let invoice_request = create_dummy_invoice_request().await;
        let result = create_invoice_info_from_request(creator_mock, invoice_request).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OfferError::AddInvoiceFailure(_)
        ));
    }

    #[tokio::test]
    async fn test_create_invoice_info_from_request_decode_payment_request_failure() {
        let mut creator_mock = MockTestBolt12InvoiceCreator::new();

        // Mock successful add_invoice
        creator_mock.expect_add_invoice().returning(|_| {
            Ok(AddInvoiceResponse {
                payment_request: "dummy_payment_request".to_string(),
                ..Default::default()
            })
        });

        // Mock decode_payment_request failure
        creator_mock
            .expect_decode_payment_request()
            .returning(|_| Err(Status::invalid_argument("Failed to decode payment request")));

        let invoice_request = create_dummy_invoice_request().await;
        let result = create_invoice_info_from_request(creator_mock, invoice_request).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OfferError::DecodePaymentRequestFailure(_)
        ));
    }

    #[tokio::test]
    async fn test_create_invoice_info_from_request_invalid_hex_payment_hash() {
        let mut creator_mock = MockTestBolt12InvoiceCreator::new();

        // Mock successful add_invoice
        creator_mock.expect_add_invoice().returning(|_| {
            Ok(AddInvoiceResponse {
                payment_request: "dummy_payment_request".to_string(),
                ..Default::default()
            })
        });

        // Mock decode_payment_request with invalid hex payment hash
        creator_mock.expect_decode_payment_request().returning(|_| {
            Ok(PayReq {
                payment_hash: "invalid_hex_string_gggg".to_string(),
                blinded_paths: vec![],
                ..Default::default()
            })
        });

        let invoice_request = create_dummy_invoice_request().await;
        let result = create_invoice_info_from_request(creator_mock, invoice_request).await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OfferError::ParsePaymentHashFailure(_)
        ));
    }
}
