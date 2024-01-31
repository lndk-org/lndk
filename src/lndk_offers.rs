use crate::lnd::MessageSigner;
use crate::OfferHandler;
use async_trait::async_trait;
use bitcoin::hashes::sha256::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::schnorr::Signature;
use bitcoin::secp256k1::{Error as Secp256k1Error, PublicKey};
use futures::executor::block_on;
use lightning::offers::invoice_request::{InvoiceRequest, UnsignedInvoiceRequest};
use lightning::offers::merkle::SignError;
use lightning::offers::offer::Offer;
use lightning::offers::parse::{Bolt12ParseError, Bolt12SemanticError};
use std::error::Error;
use std::fmt::Display;
use tokio::task;
use tonic_lnd::signrpc::{KeyLocator, SignMessageReq};
use tonic_lnd::tonic::Status;
use tonic_lnd::Client;

#[derive(Debug)]
/// OfferError is an error that occurs during the process of paying an offer.
pub(crate) enum OfferError<Secp256k1Error> {
    /// BuildUIRFailure indicates a failure to build the unsigned invoice request.
    BuildUIRFailure(Bolt12SemanticError),
    /// SignError indicates a failure to sign the invoice request.
    SignError(SignError<Secp256k1Error>),
    /// DeriveKeyFailure indicates a failure to derive key for signing the invoice request.
    DeriveKeyFailure(Status),
}

impl Display for OfferError<Secp256k1Error> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfferError::BuildUIRFailure(e) => write!(f, "Error building invoice request: {e:?}"),
            OfferError::SignError(e) => write!(f, "Error signing invoice request: {e:?}"),
            OfferError::DeriveKeyFailure(e) => write!(f, "Error signing invoice request: {e:?}"),
        }
    }
}

impl Error for OfferError<Secp256k1Error> {}

// Decodes a bech32 string into an LDK offer.
pub fn decode(offer_str: String) -> Result<Offer, Bolt12ParseError> {
    offer_str.parse::<Offer>()
}

impl OfferHandler {
    #[allow(dead_code)]
    // create_request_invoice builds and signs an invoice request, the first step in the BOLT 12 process of paying an offer.
    pub(crate) async fn create_request_invoice(
        &self,
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
        let pubkey =
            PublicKey::from_slice(&pubkey_bytes).expect("failed to deserialize public key");

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

                let signature =
                    block_on(signer.sign_message(key_loc, tagged_hash.merkle_root(), tag))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::mock;
    use std::str::FromStr;

    fn get_offer() -> String {
        "lno1qgsqvgnwgcg35z6ee2h3yczraddm72xrfua9uve2rlrm9deu7xyfzrcgqgn3qzsyvfkx26qkyypvr5hfx60h9w9k934lt8s2n6zc0wwtgqlulw7dythr83dqx8tzumg".to_string()
    }

    fn get_pubkey() -> String {
        "0313ba7ccbd754c117962b9afab6c2870eb3ef43f364a9f6c43d0fabb4553776ba".to_string()
    }

    fn get_signature() -> String {
        "28b937976a29c15827433086440b36c2bec6ca5bd977557972dca8641cd59ffba50daafb8ee99a19c950976b46f47d9e7aa716652e5657dfc555b82eff467f18".to_string()
    }

    mock! {
        TestBolt12Signer{}

         #[async_trait]
         impl MessageSigner for TestBolt12Signer {
             async fn derive_key(&mut self, key_loc: KeyLocator) -> Result<Vec<u8>, Status>;
             async fn sign_message(&mut self, key_loc: KeyLocator, merkle_hash: Hash, tag: String) -> Result<Vec<u8>, Status>;
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
        let handler = OfferHandler::new();
        assert!(handler
            .create_request_invoice(signer_mock, offer, vec![], Network::Regtest, 10000)
            .await
            .is_ok())
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
        let handler = OfferHandler::new();
        assert!(handler
            .create_request_invoice(signer_mock, offer, vec![], Network::Regtest, 10000)
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
            .expect_sign_message()
            .returning(|_, _, _| Err(Status::unknown("error testing")));

        let offer = decode(get_offer()).unwrap();
        let handler = OfferHandler::new();
        assert!(handler
            .create_request_invoice(signer_mock, offer, vec![], Network::Regtest, 10000)
            .await
            .is_err())
    }
}
