use crate::OfferError;
use async_trait::async_trait;
use bitcoin::bech32::u5;
use bitcoin::hashes::sha256::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{self, Error as Secp256k1Error, PublicKey, Scalar, Secp256k1};
use futures::executor::block_on;
use lightning::blinded_path::BlindedPath;
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::offers::invoice::UnsignedBolt12Invoice;
use lightning::offers::invoice_request::{InvoiceRequest, UnsignedInvoiceRequest};
use lightning::sign::{KeyMaterial, NodeSigner, Recipient};
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use tonic_lnd::lnrpc::{HtlcAttempt, LightningNode, ListPeersResponse, QueryRoutesResponse, Route};
use tonic_lnd::signrpc::KeyLocator;
use tonic_lnd::tonic::Status;
use tonic_lnd::{Client, ConnectError};

const ONION_MESSAGES_REQUIRED: u32 = 38;
pub(crate) const ONION_MESSAGES_OPTIONAL: u32 = 39;

/// get_lnd_client connects to LND's grpc api using the config provided, blocking until a connection is established.
pub fn get_lnd_client(cfg: LndCfg) -> Result<Client, ConnectError> {
    block_on(tonic_lnd::connect(cfg.address, cfg.cert, cfg.macaroon))
}

/// LndCfg specifies the configuration required to connect to LND's grpc client.
#[derive(Clone)]
pub struct LndCfg {
    address: String,
    cert: PathBuf,
    macaroon: PathBuf,
}

impl LndCfg {
    pub fn new(address: String, cert: PathBuf, macaroon: PathBuf) -> LndCfg {
        LndCfg {
            address,
            cert,
            macaroon,
        }
    }
}

/// features_support_onion_messages returns a boolean indicating whether a feature set supports onion messaging.
pub fn features_support_onion_messages(features: &HashMap<u32, tonic_lnd::lnrpc::Feature>) -> bool {
    features.contains_key(&ONION_MESSAGES_OPTIONAL)
        || features.contains_key(&ONION_MESSAGES_REQUIRED)
}

/// LndNodeSigner provides signing operations using LND's signer subserver.
pub(crate) struct LndNodeSigner<'a> {
    pubkey: PublicKey,
    secp_ctx: Secp256k1<secp256k1::All>,
    signer: RefCell<&'a mut tonic_lnd::SignerClient>,
}

impl<'a> LndNodeSigner<'a> {
    pub(crate) fn new(pubkey: PublicKey, signer: &'a mut tonic_lnd::SignerClient) -> Self {
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

    fn sign_bolt12_invoice_request(
        &self,
        _: &UnsignedInvoiceRequest,
    ) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
        unimplemented!("not required for onion messaging")
    }

    fn sign_bolt12_invoice(
        &self,
        _: &UnsignedBolt12Invoice,
    ) -> Result<bitcoin::secp256k1::schnorr::Signature, ()> {
        unimplemented!("not required for onion messaging")
    }

    fn sign_gossip_message(&self, _msg: UnsignedGossipMessage) -> Result<Signature, ()> {
        unimplemented!("not required for onion messaging");
    }
}

#[derive(Debug)]
/// Error when parsing provided configuration options.
pub enum NetworkParseError {
    /// Invalid indicates an invalid network was provided.
    Invalid(String),
}

impl Error for NetworkParseError {}

impl fmt::Display for NetworkParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            NetworkParseError::Invalid(network_str) => write!(f, "invalid network provided: {network_str}. Should be mainnet, testnet, signet, or regtest."),
        }
    }
}

pub fn string_to_network(network_str: &str) -> Result<Network, NetworkParseError> {
    let network_lowercase = String::from(network_str).to_lowercase();
    let network_str = network_lowercase.as_str();
    match network_str {
        "mainnet" => Ok(Network::Bitcoin),
        "testnet" => Ok(Network::Testnet),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        _ => Err(NetworkParseError::Invalid(network_str.to_string())),
    }
}

/// MessageSigner provides a layer of abstraction over the LND API for message signing.
#[async_trait]
pub trait MessageSigner {
    async fn derive_key(&mut self, key_loc: KeyLocator) -> Result<Vec<u8>, Status>;
    async fn sign_message(
        &mut self,
        key_loc: KeyLocator,
        merkle_hash: Hash,
        tag: String,
    ) -> Result<Vec<u8>, Status>;
    fn sign_uir(
        &mut self,
        key_loc: KeyLocator,
        unsigned_invoice_req: UnsignedInvoiceRequest,
    ) -> Result<InvoiceRequest, OfferError<bitcoin::secp256k1::Error>>;
}

/// PeerConnector provides a layer of abstraction over the LND API for connecting to a peer.
#[async_trait]
pub trait PeerConnector {
    async fn list_peers(&mut self) -> Result<ListPeersResponse, Status>;
    async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
    async fn get_node_info(&mut self, pub_key: String) -> Result<Option<LightningNode>, Status>;
}

/// InvoicePayer provides a layer of abstraction over the LND API for paying for a BOLT 12 invoice.
#[async_trait]
pub trait InvoicePayer {
    async fn query_routes(
        &mut self,
        path: BlindedPath,
        cltv_expiry_delta: u16,
        fee_base_msat: u32,
        fee_ppm: u32,
        msats: u64,
    ) -> Result<QueryRoutesResponse, Status>;
    async fn send_to_route(
        &mut self,
        payment_hash: [u8; 32],
        route: Route,
    ) -> Result<HtlcAttempt, Status>;
    async fn track_payment(
        &mut self,
        payment_hash: [u8; 32],
    ) -> Result<(), OfferError<Secp256k1Error>>;
}
