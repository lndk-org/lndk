use crate::OfferError;
use async_trait::async_trait;
use bitcoin::bech32::u5;
use bitcoin::hashes::sha256::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{self, PublicKey, Scalar, Secp256k1};
use futures::executor::block_on;
use lightning::blinded_path::BlindedPath;
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::offers::invoice::UnsignedBolt12Invoice;
use lightning::offers::invoice_request::{InvoiceRequest, UnsignedInvoiceRequest};
use lightning::sign::{KeyMaterial, NodeSigner, Recipient};
use log::error;
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::Display;
use std::path::PathBuf;
use std::{fmt, fs};
use tonic_lnd::lnrpc::{
    GetInfoResponse, HtlcAttempt, LightningNode, ListPeersResponse, Payment, QueryRoutesResponse,
    Route,
};
use tonic_lnd::signrpc::KeyLocator;
use tonic_lnd::tonic::Status;
use tonic_lnd::{Client, ConnectError};

const ONION_MESSAGES_REQUIRED: u32 = 38;
pub(crate) const ONION_MESSAGES_OPTIONAL: u32 = 39;

/// get_lnd_client connects to LND's grpc api using the config provided, blocking until a connection
/// is established.
pub fn get_lnd_client(cfg: LndCfg) -> Result<Client, ConnectError> {
    match cfg.creds {
        Creds::Path { macaroon, cert } => block_on(tonic_lnd::connect(cfg.address, cert, macaroon)),
        Creds::String { macaroon, cert } => {
            block_on(tonic_lnd::connect_from_memory(cfg.address, cert, macaroon))
        }
    }
}

/// LndCfg specifies the configuration required to connect to LND's grpc client.
#[derive(Clone)]
pub struct LndCfg {
    pub address: String,
    pub creds: Creds,
}

impl LndCfg {
    pub fn new(address: String, creds: Creds) -> LndCfg {
        Self { address, creds }
    }
}

pub fn validate_lnd_creds(
    cert_path: Option<PathBuf>,
    cert_pem: Option<String>,
    macaroon_path: Option<PathBuf>,
    macaroon_hex: Option<String>,
) -> Result<Creds, ValidationError> {
    if cert_path.is_none() && cert_pem.is_none() {
        return Err(ValidationError::CertRequired);
    }
    if cert_path.is_some() && cert_pem.is_some() {
        return Err(ValidationError::CertOnlyOne);
    }
    if macaroon_path.is_none() && macaroon_hex.is_none() {
        return Err(ValidationError::MacaroonRequired);
    }
    if macaroon_path.is_some() && macaroon_hex.is_some() {
        return Err(ValidationError::MacaroonOnlyOne);
    }

    let creds = match cert_path {
        Some(cert_path) => {
            let macaroon_path = match macaroon_path {
                Some(path) => path,
                None => return Err(ValidationError::CredMismatch),
            };

            Creds::Path {
                macaroon: macaroon_path,
                cert: cert_path,
            }
        }
        // If the cert_path wasn't provided, the credentials must have been loaded directly
        // instead.
        None => {
            let macaroon_hex = match macaroon_hex {
                Some(path) => path,
                None => return Err(ValidationError::CredMismatch),
            };

            Creds::String {
                cert: cert_pem.unwrap(),
                macaroon: macaroon_hex,
            }
        }
    };

    Ok(creds)
}

#[derive(Debug, PartialEq)]
pub enum ValidationError {
    CertOnlyOne,
    MacaroonOnlyOne,
    CertRequired,
    MacaroonRequired,
    CredMismatch,
}

impl Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::CertOnlyOne => {
                write!(f, "Only one of `cert_path` or `cert_pem` should be set.")
            }
            ValidationError::MacaroonOnlyOne => {
                write!(
                    f,
                    "Only one of `macaroon_path` or `macaroon_hex` should be set."
                )
            }
            ValidationError::CertRequired => {
                write!(
                    f,
                    "Cert config option is required. Please set either `cert_path` or `cert_pem`."
                )
            }
            ValidationError::MacaroonRequired => {
                write!(
                    f,
                    "Macaroon config option is required. Please set either `macaroon_path`
                    or `macaroon_hex`."
                )
            }
            ValidationError::CredMismatch => {
                write!(
                    f,
                    "There are two options for loading credentials: either both `cert_path` and `macaroon_path`
                    must be set or both `cert_pem` and `macaroon_hex` must be set. It's not possible to
                    mismatch them."
                )
            }
        }
    }
}

impl Error for ValidationError {}

#[derive(Clone, Debug)]
pub enum Creds {
    // Absolute paths to the macaroon and certificate.
    Path { macaroon: PathBuf, cert: PathBuf },

    // If the credentials are passed into lndk directly:
    // The certificate is a pem-encoded string.
    // The macaroon is a hex-encoded string.
    String { macaroon: String, cert: String },
}

impl Creds {
    #[allow(clippy::result_unit_err)]
    pub fn get_certificate_string(&self) -> Result<String, ()> {
        let cert = match self {
            Creds::Path { macaroon: _, cert } => fs::read_to_string(cert)
                .map_err(|e| error!("Error reading tls certificate from file {e:?}"))?,
            Creds::String { macaroon: _, cert } => cert.clone(),
        };
        Ok(cert)
    }
}

/// features_support_onion_messages returns a boolean indicating whether a feature set supports
/// onion messaging.
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

// get_network grabs what network lnd is running on from the LND API.
pub async fn get_network(info: GetInfoResponse) -> Result<Network, ()> {
    let mut network_str = None;
    #[allow(deprecated)]
    for chain in info.chains {
        if chain.chain == "bitcoin" {
            network_str = Some(chain.network.clone())
        }
    }
    if network_str.is_none() {
        error!("lnd node is not connected to bitcoin network as expected");
        return Err(());
    }
    Ok(string_to_network(&network_str.unwrap()).unwrap())
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
    ) -> Result<InvoiceRequest, OfferError>;
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
    async fn track_payment(&mut self, payment_hash: [u8; 32]) -> Result<Payment, OfferError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_tls_cert_string() -> String {
        "-----BEGIN CERTIFICATE-----
        MIICPjCCAeWgAwIBAgIRAOVUb/egQpSY8dEepkODgfMwCgYIKoZIzj0EAwIwMTEf
        MB0GA1UEChMWbG5kIGF1dG9nZW5lcmF0ZWQgY2VydDEOMAwGA1UEAxMFYWxpY2Uw
        HhcNMjQwNTIwMDQzNjMxWhcNMjUwNzE1MDQzNjMxWjAxMR8wHQYDVQQKExZsbmQg
        YXV0b2dlbmVyYXRlZCBjZXJ0MQ4wDAYDVQQDEwVhbGljZTBZMBMGByqGSM49AgEG
        CCqGSM49AwEHA0IABBO9DVcCiVsQrD/E/jVcsYIM22pgm2nE9ZSRqBKqo5SiNfla
        ghZS5TUKFwZx1++PcNOscRvIoWHCgYTfVB1Kcomjgd0wgdowDgYDVR0PAQH/BAQD
        AgKkMBMGA1UdJQQMMAoGCCsGAQUFBwMBMA8GA1UdEwEB/wQFMAMBAf8wHQYDVR0O
        BBYEFE+5VBJNicj149sOPIUB6nlExxoUMIGCBgNVHREEezB5ggVhbGljZYIJbG9j
        YWxob3N0ggVhbGljZYIPcG9sYXItbjE4LWFsaWNlghRob3N0LmRvY2tlci5pbnRl
        cm5hbIIEdW5peIIKdW5peHBhY2tldIIHYnVmY29ubocEfwAAAYcQAAAAAAAAAAAA
        AAAAAAAAAYcErBIAAjAKBggqhkjOPQQDAgNHADBEAiB3g1IM7JN8naboohBpY38k
        R1QvG3K8RVfshyPlTqIBagIgY+3ZNX/bffOtMQxhx5+E8csKI6LZ3e6UeEEauiZE
        aUI=
        -----END CERTIFICATE-----"
            .to_string()
    }

    fn get_macaroon_string() -> String {
        "0201036c6e6402f801030a103b7a55a2bb5810a264429f9ecf7dbdd51201301a160a0761646472657373120472656164120577726974651a130a04696e666f120472656164120577726974651a170a08696e766f69636573120472656164120577726974651a210a086d616361726f6f6e120867656e6572617465120472656164120577726974651a160a076d657373616765120472656164120577726974651a170a086f6666636861696e120472656164120577726974651a160a076f6e636861696e120472656164120577726974651a140a057065657273120472656164120577726974651a180a067369676e6572120867656e6572617465120472656164000006203ad31d52e046455bae5c3343fa4753dc1659065e8fc4e8f56aec4dbd309d4463".to_string()
    }

    #[tokio::test]
    async fn test_validate_lnd_creds_path_option() {
        let cert_path = Some(PathBuf::new());
        let macaroon_path = Some(PathBuf::new());

        assert!(validate_lnd_creds(cert_path, None, macaroon_path, None).is_ok())
    }

    #[tokio::test]
    async fn test_validate_lnd_creds_directly_option() {
        let cert_pem = Some(get_tls_cert_string());
        let macaroon_hex = Some(get_macaroon_string());

        assert!(validate_lnd_creds(None, cert_pem, None, macaroon_hex).is_ok())
    }

    // Test that when the provided credentials are mismatched, we get the correct error.
    #[tokio::test]
    async fn test_validate_lnd_creds_mismatch() {
        let cert_pem = Some(get_tls_cert_string());
        let macaroon_path = Some(PathBuf::new());

        assert_eq!(
            validate_lnd_creds(None, cert_pem, macaroon_path, None).unwrap_err(),
            ValidationError::CredMismatch
        );

        let cert_path = Some(PathBuf::new());
        let macaroon_hex = Some(get_macaroon_string());

        assert_eq!(
            validate_lnd_creds(cert_path, None, None, macaroon_hex).unwrap_err(),
            ValidationError::CredMismatch
        )
    }

    // Test that when none or too few credentials are provided, we get the correct error.
    #[tokio::test]
    async fn test_validate_lnd_creds_required_error() {
        let cert_pem = Some(get_tls_cert_string());
        assert_eq!(
            validate_lnd_creds(None, cert_pem, None, None).unwrap_err(),
            ValidationError::MacaroonRequired
        );

        let macaroon_hex = Some(get_macaroon_string());
        assert_eq!(
            validate_lnd_creds(None, None, None, macaroon_hex).unwrap_err(),
            ValidationError::CertRequired
        )
    }

    #[tokio::test]
    async fn test_validate_lnd_creds_only_one() {
        let cert_path = Some(PathBuf::new());
        let cert_pem = Some(get_tls_cert_string());
        let macaroon_path = Some(PathBuf::new());
        let macaroon_hex = Some(get_macaroon_string());

        assert_eq!(
            validate_lnd_creds(cert_path.clone(), cert_pem, macaroon_path.clone(), None)
                .unwrap_err(),
            ValidationError::CertOnlyOne
        );

        assert_eq!(
            validate_lnd_creds(cert_path, None, macaroon_path, macaroon_hex).unwrap_err(),
            ValidationError::MacaroonOnlyOne
        )
    }
}
