use crate::lnd::{LndNodeSigner, MessageSigner, ONION_MESSAGES_OPTIONAL};
use crate::onion_messenger::{
    lookup_onion_support, relay_outgoing_msg_event, MessengerUtilities, SendCustomMessage,
};
use async_trait::async_trait;
use bitcoin::blockdata::constants::ChainHash;
use bitcoin::hashes::sha256::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::schnorr::Signature;
use bitcoin::secp256k1::{Error as Secp256k1Error, PublicKey};
use core::ops::Deref;
use futures::executor::block_on;
use lightning::blinded_path::BlindedPath;
use lightning::ln::features::InitFeatures;
use lightning::ln::msgs::{Init, OnionMessageHandler};
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::offers::invoice_request::{InvoiceRequest, UnsignedInvoiceRequest};
use lightning::offers::merkle::SignError;
use lightning::offers::offer::Offer;
use lightning::offers::parse::{Bolt12ParseError, Bolt12SemanticError};
use lightning::onion_message::{
    CustomOnionMessageHandler, DefaultMessageRouter, Destination, MessageRouter, OffersMessage,
    OffersMessageHandler, OnionMessagePath, OnionMessenger, SendError,
};
use lightning::sign::EntropySource;
use lightning::sign::NodeSigner;
use lightning::util::logger::Logger;
use std::error::Error;
use std::fmt::Display;
use tokio::task;
use tonic_lnd::signrpc::{KeyLocator, SignMessageReq};
use tonic_lnd::tonic::Status;
use tonic_lnd::Client;

#[derive(Debug)]
/// OfferError is an error that occurs during the process of paying an offer.
pub enum OfferError<Secp256k1Error> {
    /// BuildUIRFailure indicates a failure to build the unsigned invoice request.
    BuildUIRFailure(Bolt12SemanticError),
    /// SignError indicates a failure to sign the invoice request.
    SignError(SignError<Secp256k1Error>),
    /// DeriveKeyFailure indicates a failure to derive key for signing the invoice request.
    DeriveKeyFailure(Status),
    /// Trouble sending an offers-related onion message along.
    SendError(SendError),
}

impl Display for OfferError<Secp256k1Error> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfferError::BuildUIRFailure(e) => write!(f, "Error building invoice request: {e:?}"),
            OfferError::SignError(e) => write!(f, "Error signing invoice request: {e:?}"),
            OfferError::DeriveKeyFailure(e) => write!(f, "Error signing invoice request: {e:?}"),
            OfferError::SendError(e) => write!(f, "Error sending onion message: {e:?}"),
        }
    }
}

impl Error for OfferError<Secp256k1Error> {}

// Decodes a bech32 string into an LDK offer.
pub fn decode(offer_str: String) -> Result<Offer, Bolt12ParseError> {
    offer_str.parse::<Offer>()
}

// request_invoice builds an invoice request and tries to send it to the offer maker.
pub async fn request_invoice(
    client: Client,
    custom_msg_client: impl SendCustomMessage,
    pubkey: PublicKey,
    offer: Offer,
    introduction_node: PublicKey,
    blinded_path: BlindedPath,
) -> Result<(), OfferError<Secp256k1Error>> {
    let mut signer_clone = client.clone();
    let node_client = signer_clone.signer();
    let node_signer = LndNodeSigner::new(pubkey, node_client);
    let messenger_utils = MessengerUtilities::new();
    let onion_messenger = OnionMessenger::new(
        &messenger_utils,
        &node_signer,
        &messenger_utils,
        &DefaultMessageRouter {},
        IgnoringMessageHandler {},
        IgnoringMessageHandler {},
    );

    // Ensure that the introduction node we need to connect to actually supports onion messaging.
    let onion_support =
        lookup_onion_support(&introduction_node.clone(), client.clone().lightning()).await;
    if !onion_support {
        return Err(OfferError::SendError(SendError::InvalidFirstHop));
    }

    let invoice_request =
        create_invoice_request(client.clone(), offer, vec![], Network::Regtest, 20000)
            .await
            .expect("should build invoice request");
    send_invoice_request(
        Network::Regtest,
        invoice_request,
        onion_messenger,
        vec![introduction_node],
        blinded_path,
        custom_msg_client,
    )
    .await
}

#[allow(dead_code)]
// create_invoice_request builds and signs an invoice request, the first step in the BOLT 12 process of paying an offer.
pub async fn create_invoice_request(
    mut signer: impl MessageSigner + std::marker::Send + 'static,
    offer: Offer,
    metadata: Vec<u8>,
    network: Network,
    msats: u64,
) -> Result<InvoiceRequest, OfferError<bitcoin::secp256k1::Error>> {
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
    let pubkey = PublicKey::from_slice(&pubkey_bytes).expect("failed to deserialize public key");

    let unsigned_invoice_req = offer
        .request_invoice(metadata, pubkey)
        .unwrap()
        .chain(network)
        .unwrap()
        .amount_msats(msats)
        .unwrap()
        .build()
        .map_err(OfferError::BuildUIRFailure)?;

    // To create a valid invoice request, we also need to sign it. This is spawned in a blocking
    // task because we need to call block_on on sign_message so that sign_closure can be a
    // synchronous closure.
    task::spawn_blocking(move || {
        let sign_closure = |msg: &UnsignedInvoiceRequest| {
            let tagged_hash = msg.as_ref();
            let tag = tagged_hash.tag().to_string();

            let signature = block_on(signer.sign_message(key_loc, tagged_hash.merkle_root(), tag))
                .map_err(|_| Secp256k1Error::InvalidSignature)?;

            Signature::from_slice(&signature)
        };

        unsigned_invoice_req
            .sign(sign_closure)
            .map_err(OfferError::SignError)
    })
    .await
    .unwrap()
}

// send_invoice_request sends the invoice request to node that created the offer we want to pay.
pub async fn send_invoice_request(
    network: Network,
    invoice_request: InvoiceRequest,
    onion_messenger: impl OnionMessageHandler + SendOnion,
    intermediate_nodes: Vec<PublicKey>,
    blinded_path: BlindedPath,
    mut custom_msg_client: impl SendCustomMessage,
) -> Result<(), OfferError<bitcoin::secp256k1::Error>> {
    // We need to make sure our local onion messenger is connected to the needed local peers in
    // so it can relay along the onion messages to them.
    let onion_message_optional: u64 = 1 << ONION_MESSAGES_OPTIONAL;
    let init_features = InitFeatures::from_le_bytes(onion_message_optional.to_le_bytes().to_vec());
    let network = vec![ChainHash::using_genesis_block(network)];
    onion_messenger
        .peer_connected(
            &intermediate_nodes[0].clone(),
            &Init {
                features: init_features,
                remote_network_address: None,
                networks: Some(network.clone()),
            },
            false,
        )
        .map_err(|_| OfferError::SendError(SendError::InvalidFirstHop))?;

    let path = OnionMessagePath {
        // For now we connect directly to the introduction node of the blinded path so we don't need any
        // intermediate nodes here. In the future we'll query for a full path to the introduction node for
        // better sender privacy.
        intermediate_nodes: vec![],
        destination: Destination::BlindedPath(blinded_path),
    };
    let contents = OffersMessage::InvoiceRequest(invoice_request);

    onion_messenger
        .send_onion(path, contents, None)
        .map_err(OfferError::<Secp256k1Error>::SendError)?;

    let onion_message = onion_messenger
        .next_onion_message_for_peer(intermediate_nodes[0])
        .expect("missing onion message");

    relay_outgoing_msg_event(
        &intermediate_nodes[0],
        onion_message,
        &mut custom_msg_client,
    )
    .await;
    Ok(())
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
            msg: merkle_root.as_ref().to_vec(),
            tag: tag_vec,
            key_loc: Some(key_loc),
            schnorr_sig: true,
            ..Default::default()
        };

        let resp = self.signer().sign_message(req).await?;

        let resp_inner = resp.into_inner();
        Ok(resp_inner.signature)
    }

    async fn signer(&mut self) -> &mut tonic_lnd::SignerClient {
        self.signer()
    }
}

pub trait SendOnion {
    fn send_onion(
        &self,
        path: OnionMessagePath,
        contents: OffersMessage,
        reply_path: Option<BlindedPath>,
    ) -> Result<(), SendError>;
}

// We have to wrap OnionMessenger's send_onion_message in order to use it as a trait when mocking for the unit tests.
impl<ES: Deref, NS: Deref, L: Deref, MR: Deref, OMH: Deref, CMH: Deref> SendOnion
    for OnionMessenger<ES, NS, L, MR, OMH, CMH>
where
    ES::Target: EntropySource,
    NS::Target: NodeSigner,
    L::Target: Logger,
    MR::Target: MessageRouter,
    OMH::Target: OffersMessageHandler,
    CMH::Target: CustomOnionMessageHandler,
{
    fn send_onion(
        &self,
        path: OnionMessagePath,
        contents: OffersMessage,
        reply_path: Option<BlindedPath>,
    ) -> Result<(), SendError> {
        self.send_onion_message(path, contents, reply_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_utils::onion_message;
    use bitcoin::secp256k1::{KeyPair, Secp256k1, SecretKey};
    use core::convert::Infallible;
    use lightning::ln::features::{InitFeatures, NodeFeatures};
    use lightning::ln::msgs::{Init, OnionMessage, OnionMessageHandler};
    use lightning::offers::offer::OfferBuilder;
    use mockall::mock;
    use std::str::FromStr;
    use tonic_lnd::lnrpc::{SendCustomMessageRequest, SendCustomMessageResponse};

    fn get_offer() -> String {
        "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzrcgqgn3qzsyvfkx26qkyypvr5hfx60h9w9k934lt8s2n6zc0wwtgqlulw7dythr83dqx8tzumg".to_string()
    }

    fn get_pubkey() -> String {
        "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba".to_string()
    }

    fn get_signature() -> String {
        "28b937976a29c15827433086440b36c2bec6ca5bd977557972dca8641cd59ffba50daafb8ee99a19c950976b46f47d9e7aa716652e5657dfc555b82eff467f18".to_string()
    }

    fn get_invoice_request() -> InvoiceRequest {
        let secp_ctx = Secp256k1::new();
        let keys = KeyPair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap());
        OfferBuilder::new("foo".into(), keys.public_key())
            .amount_msats(1000)
            .build()
            .unwrap()
            .request_invoice(vec![1; 32], keys.public_key())
            .unwrap()
            .build()
            .unwrap()
            .sign::<_, Infallible>(|message| {
                Ok(secp_ctx.sign_schnorr_no_aux_rand(message.as_ref().as_digest(), &keys))
            })
            .unwrap()
    }

    // This struct for building a blinded path is shamelessly taken from the rust-lightning repo tests.
    // Credit where it's due, kek.
    struct Randomness;

    impl EntropySource for Randomness {
        fn get_secure_random_bytes(&self) -> [u8; 32] {
            [42; 32]
        }
    }

    fn pubkey(byte: u8) -> PublicKey {
        let secp_ctx = Secp256k1::new();
        PublicKey::from_secret_key(&secp_ctx, &privkey(byte))
    }

    fn privkey(byte: u8) -> SecretKey {
        SecretKey::from_slice(&[byte; 32]).unwrap()
    }

    fn get_blinded_path() -> BlindedPath {
        let entropy_source = Randomness {};
        let secp_ctx = Secp256k1::new();
        BlindedPath::new_for_message(
            &[pubkey(43), pubkey(44), pubkey(42)],
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
             async fn signer(&mut self) -> &mut tonic_lnd::SignerClient;
         }
    }

    mock! {
        OnionHandler{}

        impl OnionMessageHandler for OnionHandler {
            fn handle_onion_message(&self, peer_node_id: &PublicKey, msg: &OnionMessage);
            fn next_onion_message_for_peer(&self, peer_node_id: PublicKey) -> Option<OnionMessage>;
            fn peer_connected(&self, their_node_id: &PublicKey, init: &Init, inbound: bool) -> Result<(), ()>;
            fn peer_disconnected(&self, their_node_id: &PublicKey);
            fn provided_node_features(&self) -> NodeFeatures;
            fn provided_init_features(&self, their_node_id: &PublicKey) -> InitFeatures;
        }

        impl SendOnion for OnionHandler {
            fn send_onion(&self,
                path: OnionMessagePath,
                contents: OffersMessage,
                reply_path: Option<BlindedPath>,
            ) -> Result<(), SendError>;
        }
    }

    mock! {
        SendCustomMessenger{}

        #[async_trait]
         impl SendCustomMessage for SendCustomMessenger{
             async fn send_custom_message(&mut self, request: SendCustomMessageRequest) -> Result<SendCustomMessageResponse, Status>;
         }
    }

    #[tokio::test]
    async fn test_request_invoice() {
        let mut signer_mock = MockTestBolt12Signer::new();

        signer_mock.expect_derive_key().returning(|_| {
            Ok(PublicKey::from_str(&get_pubkey())
                .unwrap()
                .serialize()
                .to_vec())
        });

        signer_mock.expect_sign_message().returning(|_, _, _| {
            Ok(Signature::from_str(&get_signature())
                .unwrap()
                .as_ref()
                .to_vec())
        });

        let offer = decode(get_offer()).unwrap();

        assert!(
            create_invoice_request(signer_mock, offer, vec![], Network::Regtest, 10000)
                .await
                .is_ok()
        )
    }

    #[tokio::test]
    async fn test_request_invoice_derive_key_error() {
        let mut signer_mock = MockTestBolt12Signer::new();

        signer_mock
            .expect_derive_key()
            .returning(|_| Err(Status::unknown("error testing")));

        signer_mock.expect_sign_message().returning(|_, _, _| {
            Ok(Signature::from_str(&get_signature())
                .unwrap()
                .as_ref()
                .to_vec())
        });

        let offer = decode(get_offer()).unwrap();

        assert!(
            create_invoice_request(signer_mock, offer, vec![], Network::Regtest, 10000)
                .await
                .is_err()
        )
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
            .expect_sign_message()
            .returning(|_, _, _| Err(Status::unknown("error testing")));

        let offer = decode(get_offer()).unwrap();

        assert!(
            create_invoice_request(signer_mock, offer, vec![], Network::Regtest, 10000)
                .await
                .is_err()
        )
    }

    #[tokio::test]
    async fn test_send_invoice_request() {
        let mut onion_mock = MockOnionHandler::new();
        let mut sender_mock = MockSendCustomMessenger::new();

        onion_mock
            .expect_peer_connected()
            .returning(|_, _, _| Ok(()));

        onion_mock.expect_send_onion().returning(|_, _, _| Ok(()));

        onion_mock
            .expect_next_onion_message_for_peer()
            .returning(|_| Some(onion_message()));

        sender_mock
            .expect_send_custom_message()
            .returning(|_| Ok(SendCustomMessageResponse {}));

        let invoice_request = get_invoice_request();
        let blinded_path = get_blinded_path();
        let pubkey = PublicKey::from_str(&get_pubkey()).unwrap();

        assert!(send_invoice_request(
            Network::Regtest,
            invoice_request,
            onion_mock,
            vec![pubkey],
            blinded_path,
            sender_mock
        )
        .await
        .is_ok())
    }

    #[tokio::test]
    async fn test_send_invoice_request_peer_connected_error() {
        let mut onion_mock = MockOnionHandler::new();
        let mut sender_mock = MockSendCustomMessenger::new();

        onion_mock
            .expect_peer_connected()
            .returning(|_, _, _| Err(()));

        onion_mock.expect_send_onion().returning(|_, _, _| Ok(()));

        onion_mock
            .expect_next_onion_message_for_peer()
            .returning(|_| Some(onion_message()));

        sender_mock
            .expect_send_custom_message()
            .returning(|_| Ok(SendCustomMessageResponse {}));

        let invoice_request = get_invoice_request();
        let blinded_path = get_blinded_path();
        let pubkey = PublicKey::from_str(&get_pubkey()).unwrap();

        assert!(send_invoice_request(
            Network::Regtest,
            invoice_request,
            onion_mock,
            vec![pubkey],
            blinded_path,
            sender_mock
        )
        .await
        .is_err())
    }

    #[tokio::test]
    async fn test_send_invoice_request_send_onion_error() {
        let mut onion_mock = MockOnionHandler::new();
        let mut sender_mock = MockSendCustomMessenger::new();

        onion_mock
            .expect_peer_connected()
            .returning(|_, _, _| Ok(()));

        onion_mock
            .expect_send_onion()
            .returning(|_, _, _| Err(SendError::InvalidFirstHop));

        onion_mock
            .expect_next_onion_message_for_peer()
            .returning(|_| Some(onion_message()));

        sender_mock
            .expect_send_custom_message()
            .returning(|_| Ok(SendCustomMessageResponse {}));

        let invoice_request = get_invoice_request();
        let blinded_path = get_blinded_path();
        let pubkey = PublicKey::from_str(&get_pubkey()).unwrap();

        assert!(send_invoice_request(
            Network::Regtest,
            invoice_request,
            onion_mock,
            vec![pubkey],
            blinded_path,
            sender_mock
        )
        .await
        .is_err())
    }
}
