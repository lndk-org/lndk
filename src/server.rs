use crate::lnd::{get_lnd_client, get_network, Creds, LndCfg};
use crate::lndk_offers::get_destination;
use crate::{
    lndkrpc, OfferError, OfferHandler, PayOfferParams, TLS_CERT_FILENAME, TLS_KEY_FILENAME,
};
use bitcoin::secp256k1::PublicKey;
use lightning::offers::offer::Offer;
use lndkrpc::offers_server::Offers;
use lndkrpc::{PayOfferRequest, PayOfferResponse};
use rcgen::{generate_simple_self_signed, CertifiedKey, Error as RcgenError};
use std::error::Error;
use std::fmt::Display;
use std::fs::{metadata, set_permissions, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tonic::metadata::MetadataMap;
use tonic::transport::Identity;
use tonic::{Request, Response, Status};
use tonic_lnd::lnrpc::GetInfoRequest;
pub struct LNDKServer {
    offer_handler: Arc<OfferHandler>,
    node_id: PublicKey,
    // The LND tls cert we need to establish a connection with LND.
    lnd_cert: String,
    address: String,
}

impl LNDKServer {
    pub async fn new(
        offer_handler: Arc<OfferHandler>,
        node_id: &str,
        lnd_cert: String,
        address: String,
    ) -> Self {
        Self {
            offer_handler,
            node_id: PublicKey::from_str(node_id).unwrap(),
            lnd_cert,
            address,
        }
    }
}

#[tonic::async_trait]
impl Offers for LNDKServer {
    async fn pay_offer(
        &self,
        request: Request<PayOfferRequest>,
    ) -> Result<Response<PayOfferResponse>, Status> {
        log::info!("Received a request: {:?}", request);

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let mut client = get_lnd_client(lnd_cfg)
            .map_err(|e| Status::unavailable(format!("Couldn't connect to lnd: {e}")))?;

        let inner_request = request.get_ref();
        let offer = Offer::from_str(&inner_request.offer).map_err(|e| {
            Status::invalid_argument(format!(
                "The provided offer was invalid. Please provide a valid offer in bech32 format,
                i.e. starting with 'lno'. Error: {e:?}"
            ))
        })?;

        let destination = get_destination(&offer).await;
        let reply_path = match self
            .offer_handler
            .create_reply_path(client.clone(), self.node_id)
            .await
        {
            Ok(reply_path) => reply_path,
            Err(e) => return Err(Status::internal(format!("Internal error: {e}"))),
        };

        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .expect("failed to get info")
            .into_inner();
        let network = get_network(info)
            .await
            .map_err(|e| Status::internal(format!("{e:?}")))?;

        let cfg = PayOfferParams {
            offer,
            amount: inner_request.amount,
            network,
            client,
            destination,
            reply_path: Some(reply_path),
            response_invoice_timeout: inner_request.response_invoice_timeout,
        };

        let payment = match self.offer_handler.pay_offer(cfg).await {
            Ok(payment) => {
                log::info!("Payment succeeded.");
                payment
            }
            Err(e) => match e {
                OfferError::AlreadyProcessing => {
                    return Err(Status::already_exists(format!("{e}")))
                }
                OfferError::InvalidAmount(e) => {
                    return Err(Status::invalid_argument(e.to_string()))
                }
                OfferError::InvalidCurrency => {
                    return Err(Status::invalid_argument(format!("{e}")))
                }
                _ => return Err(Status::internal(format!("Internal error: {e}"))),
            },
        };

        let reply = PayOfferResponse {
            payment_preimage: payment.payment_preimage,
        };

        Ok(Response::new(reply))
    }
}

// We need to check that the client passes in a tls cert pem string, hexadecimal macaroon,
// and address, so they can connect to LND.
fn check_auth_metadata(metadata: &MetadataMap) -> Result<String, Status> {
    let macaroon = match metadata.get("macaroon") {
        Some(macaroon_hex) => macaroon_hex
            .to_str()
            .map_err(|e| {
                Status::invalid_argument(format!("Invalid macaroon string provided: {e}"))
            })?
            .to_string(),
        _ => {
            return Err(Status::unauthenticated(
                "No LND macaroon provided: Make sure to provide macaroon in request metadata",
            ))
        }
    };

    Ok(macaroon)
}

/// An error that occurs when generating TLS credentials.
#[derive(Debug)]
pub enum CertificateGenFailure {
    RcgenError(RcgenError),
    IoError(std::io::Error),
}

impl Display for CertificateGenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertificateGenFailure::RcgenError(e) => {
                write!(f, "Error generating TLS certificate: {e:?}")
            }
            CertificateGenFailure::IoError(e) => write!(f, "IO error: {e:?}"),
        }
    }
}

impl Error for CertificateGenFailure {}

// If a tls cert/key pair doesn't already exist, generate_tls_creds creates the tls cert/key pair
// required secure connections to LNDK's gRPC server. By default they are stored in ~/.lndk.
pub fn generate_tls_creds(data_dir: PathBuf) -> Result<(), CertificateGenFailure> {
    let cert_path = data_dir.join(TLS_CERT_FILENAME);
    let key_path = data_dir.join(TLS_KEY_FILENAME);

    // Did we have to generate a new key? In that case we also need to regenerate the certificate.
    if !key_path.exists() || !cert_path.exists() {
        log::debug!("Generating fresh TLS credentials in {data_dir:?}");
        let subject_alt_names = vec!["localhost".to_string()];

        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(subject_alt_names)
            .map_err(CertificateGenFailure::RcgenError)?;

        // Create the tls files. Make sure the key is user-readable only:
        let mut file = File::create(&key_path).map_err(CertificateGenFailure::IoError)?;
        let mut perms = metadata(&key_path)
            .map_err(CertificateGenFailure::IoError)?
            .permissions();
        perms.set_mode(0o600);
        set_permissions(&key_path, perms).map_err(CertificateGenFailure::IoError)?;

        file.write_all(key_pair.serialize_pem().as_bytes())
            .map_err(CertificateGenFailure::IoError)?;
        drop(file);

        std::fs::write(&cert_path, cert.pem()).map_err(CertificateGenFailure::IoError)?;
    };

    Ok(())
}

// Read the existing tls credentials from disk.
pub fn read_tls(data_dir: PathBuf) -> Result<Identity, std::io::Error> {
    let cert = std::fs::read_to_string(data_dir.join(TLS_CERT_FILENAME))?;
    let key = std::fs::read_to_string(data_dir.join(TLS_KEY_FILENAME))?;

    Ok(Identity::from_pem(cert, key))
}
