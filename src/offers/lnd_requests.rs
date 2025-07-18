use std::str::FromStr;

use bitcoin::{key::Secp256k1, secp256k1::PublicKey, Network};
use lightning::{
    blinded_path::{
        message::{BlindedMessagePath, MessageContext, OffersContext},
        Direction, IntroductionNode,
    },
    ln::{
        channelmanager::{PaymentId, Verification},
        inbound_payment::ExpandedKey,
    },
    offers::{invoice_request::InvoiceRequest, offer::Offer},
    onion_message::{
        messenger::{Destination, MessageSendInstructions},
        offers::OffersMessage,
    },
    sign::EntropySource,
};
use log::{debug, error, trace};
use tonic_lnd::{
    lnrpc::{ChanInfoRequest, GetInfoRequest, Payment},
    Client,
};

use crate::{
    lnd::{features_support_onion_messages, InvoicePayer, PeerConnector},
    offers::handler::SendPaymentParams,
    onion_messenger::MessengerUtilities,
};

use super::{validate_amount, OfferError};

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
        )
        .await
        .map_err(OfferError::RouteFailure)?;

    let _ = payer
        .send_to_route(params.payment_hash, resp.routes[0].clone())
        .await
        .map_err(OfferError::RouteFailure)?;

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

/// create_reply_path creates a blinded path to provide to the offer node when requesting an
/// invoice so they know where to send the invoice back to. We try to find a peer that we're
/// connected to with the necessary requirements to form a blinded path. The peer needs two
/// things:
/// 1) Onion messaging support.
/// 2) To be an advertised node with at least one public channel.
///
/// Otherwise we create a blinded path directly to ourselves.
pub async fn create_reply_path(
    mut connector: impl PeerConnector + std::marker::Send + 'static,
    node_id: PublicKey,
    message_context: MessageContext,
    messenger_utils: &MessengerUtilities,
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
        Destination::Node(pubkey) => connect_to_peer(client.clone(), pubkey).await?,
        Destination::BlindedPath(ref path) => match path.introduction_node() {
            IntroductionNode::NodeId(pubkey) => connect_to_peer(client.clone(), *pubkey).await?,
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
    let reply_path =
        create_reply_path(client.clone(), pubkey, message_context, messenger_utils).await?;

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

async fn connect_to_peer(
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

    connector
        .connect_peer(node_id_str, node.addresses[0].clone().addr)
        .await
        .map_err(OfferError::PeerConnectError)?;

    Ok(())
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
    use std::collections::HashMap;

    use super::*;
    use crate::offers::client_impls::tests::{MockTestInvoicePayer, MockTestPeerConnector};
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
            ChannelEdge, HtlcAttempt, LightningNode, ListPeersResponse, NodeAddress, NodeInfo,
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
    // Test that create_reply_path works fine when no suitable introduction node peer is found.
    async fn test_create_reply_path_no_intro_node() {
        let mut connector_mock = MockTestPeerConnector::new();

        connector_mock
            .expect_list_peers()
            .returning(|| Ok(ListPeersResponse { peers: vec![] }));

        let receiver_node_id = PublicKey::from_str(&get_pubkeys()[0]).unwrap();
        let message_context = get_message_context();
        let response = create_reply_path(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_ok());
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
        let response = create_reply_path(
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
        let response = create_reply_path(
            connector_mock,
            receiver_node_id,
            message_context,
            &MessengerUtilities::new([42; 32]),
        )
        .await;
        assert!(response.is_err());
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
        let response = create_reply_path(
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
        let response = create_reply_path(
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
        assert!(send_payment(payer_mock, params).await.is_ok());
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
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash: payment_hash,
            msats: 2000,
            payment_id,
        };
        assert!(send_payment(payer_mock, params).await.is_err());
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
        let params = SendPaymentParams {
            path: blinded_path,
            cltv_expiry_delta: 200,
            fee_base_msat: 1,
            fee_ppm: 0,
            payment_hash: payment_hash,
            msats: 2000,
            payment_id,
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
}
