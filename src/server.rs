use crate::lnd::{get_lnd_client, get_network, Creds, LndCfg};
use crate::lndk_offers::get_destination;
use crate::{lndkrpc, OfferError, OfferHandler, PayOfferParams};
use bitcoin::secp256k1::PublicKey;
use lightning::offers::offer::Offer;
use lndkrpc::offers_server::Offers;
use lndkrpc::{PayOfferRequest, PayOfferResponse};
use std::str::FromStr;
use std::sync::Arc;
use tonic::metadata::MetadataMap;
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
            .create_reply_path(client.clone(), self.node_id, offer.signing_pubkey())
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
