use crate::lnd::{get_lnd_client, get_network, Creds, LndCfg, LndError};
use crate::lndkrpc::{CreateOfferRequest, CreateOfferResponse};
use crate::offers::get_destination;
use crate::offers::handler::{CreateOfferParams, PayOfferParams};
use crate::offers::validate_amount;
use crate::offers::OfferError;
use crate::{lndkrpc, Bolt12InvoiceString, OfferHandler};
use bitcoin::secp256k1::PublicKey;
use lightning::blinded_path::payment::BlindedPaymentPath;
use lightning::blinded_path::{Direction, IntroductionNode};
use lightning::ln::channelmanager::PaymentId;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::{Offer, Quantity};
use lightning::sign::EntropySource;
use lightning::util::ser::Writeable;
use lndkrpc::offers_server::Offers;
use lndkrpc::{
    Bolt12InvoiceContents, DecodeInvoiceRequest, FeatureBit, GetInvoiceRequest, GetInvoiceResponse,
    PayInvoiceRequest, PayInvoiceResponse, PayOfferRequest, PayOfferResponse, PaymentHash,
    PaymentPaths,
};
use std::num::NonZeroU64;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tonic::metadata::MetadataMap;
use tonic::{Request, Response, Status};
use tonic_lnd::lnrpc::GetInfoRequest;

pub struct LNDKServer {
    offer_handler: Arc<OfferHandler>,
    #[allow(dead_code)]
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
        log::info!("Received a request: {:?}", request.get_ref());

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let mut client = get_lnd_client(lnd_cfg)?;

        let inner_request = request.get_ref();
        let offer = Offer::from_str(&inner_request.offer).map_err(OfferError::ParseOfferFailure)?;

        let destination = get_destination(&offer).await?;
        let reply_path = None;
        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(LndError::ServiceUnavailable)?
            .into_inner();
        let network = get_network(info).await?;

        let fee_limit = create_fee_limit(inner_request.fee_limit, inner_request.fee_limit_percent);

        let cfg = PayOfferParams {
            offer,
            amount: inner_request.amount,
            payer_note: inner_request.payer_note.clone(),
            network,
            client,
            destination,
            reply_path,
            response_invoice_timeout: inner_request.response_invoice_timeout,
            fee_limit,
        };

        let payment = self.offer_handler.pay_offer(cfg).await?;
        log::info!(
            "Payment succeeded with preimage: {}",
            payment.payment_preimage
        );

        let reply = PayOfferResponse {
            payment_preimage: payment.payment_preimage,
        };

        Ok(Response::new(reply))
    }

    async fn decode_invoice(
        &self,
        request: Request<DecodeInvoiceRequest>,
    ) -> Result<Response<Bolt12InvoiceContents>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let invoice_string: Bolt12InvoiceString = request.get_ref().invoice.clone().into();
        let invoice = Bolt12Invoice::try_from(invoice_string)?;

        let reply: Bolt12InvoiceContents = generate_bolt12_invoice_contents(&invoice);
        Ok(Response::new(reply))
    }

    async fn get_invoice(
        &self,
        request: Request<GetInvoiceRequest>,
    ) -> Result<Response<GetInvoiceResponse>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let mut client = get_lnd_client(lnd_cfg)?;

        let inner_request = request.get_ref();
        let offer = Offer::from_str(&inner_request.offer).map_err(OfferError::ParseOfferFailure)?;

        let destination = get_destination(&offer).await?;
        let reply_path = None;

        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(LndError::ServiceUnavailable)?
            .into_inner();
        let network = get_network(info).await?;

        let cfg = PayOfferParams {
            offer,
            amount: inner_request.amount,
            payer_note: inner_request.payer_note.clone(),
            network,
            client,
            destination,
            reply_path,
            response_invoice_timeout: inner_request.response_invoice_timeout,
            fee_limit: None,
        };

        let (invoice, _, payment_id) = self.offer_handler.get_invoice(cfg).await?;
        log::info!("Invoice request succeeded.");

        // We need to remove the payment from our tracking map now.
        // TODO: This is a hack to remove the payment from the tracking map. We should do it when
        // get_invoice params option or other way.
        {
            self.offer_handler.remove_active_payment(payment_id);
        }

        let reply: GetInvoiceResponse = GetInvoiceResponse {
            invoice_hex_str: encode_invoice_as_hex(&invoice)?,
            invoice_contents: Some(generate_bolt12_invoice_contents(&invoice)),
        };

        Ok(Response::new(reply))
    }

    async fn pay_invoice(
        &self,
        request: Request<PayInvoiceRequest>,
    ) -> Result<Response<PayInvoiceResponse>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let client = get_lnd_client(lnd_cfg)?;

        let inner_request = request.get_ref();
        let invoice_string: Bolt12InvoiceString = inner_request.invoice.clone().into();
        let invoice = Bolt12Invoice::try_from(invoice_string)?;

        let amount = validate_amount(invoice.amount().as_ref(), inner_request.amount).await?;
        let payment_id = PaymentId(self.offer_handler.messenger_utils.get_secure_random_bytes());

        let fee_limit = create_fee_limit(inner_request.fee_limit, inner_request.fee_limit_percent);

        let invoice = self
            .offer_handler
            .pay_invoice(client, amount, &invoice, payment_id, fee_limit)
            .await?;
        log::info!("Invoice paid.");

        let reply = PayInvoiceResponse {
            payment_preimage: invoice.payment_preimage,
        };

        Ok(Response::new(reply))
    }

    async fn create_offer(
        &self,
        request: Request<CreateOfferRequest>,
    ) -> Result<Response<CreateOfferResponse>, Status> {
        log::info!("Received a request: {:?}", request.get_ref());

        let metadata = request.metadata();
        let macaroon = check_auth_metadata(metadata)?;
        let creds = Creds::String {
            cert: self.lnd_cert.clone(),
            macaroon,
        };
        let lnd_cfg = LndCfg::new(self.address.clone(), creds);
        let mut client = get_lnd_client(lnd_cfg)?;
        let inner_request = request.get_ref();
        let info = client
            .lightning()
            .get_info(GetInfoRequest {})
            .await
            .map_err(LndError::ServiceUnavailable)?
            .into_inner();
        let network = get_network(info).await?;
        let quantity = parse_quantity(inner_request.quantity);

        let request = CreateOfferParams {
            client,
            amount_msats: inner_request.amount.unwrap_or(0),
            chain: network,
            description: inner_request.description.clone(),
            issuer: inner_request.issuer.clone(),
            quantity,
            expiry: inner_request.expiry.map(Duration::from_secs),
        };
        let offer = self.offer_handler.create_offer(request).await?;

        let reply = CreateOfferResponse {
            offer: offer.to_string(),
        };
        Ok(Response::new(reply))
    }
}

fn parse_quantity(rpc_quantity: Option<u64>) -> Option<Quantity> {
    match rpc_quantity {
        None => None,
        Some(0) => Some(Quantity::Unbounded),
        Some(1) => Some(Quantity::One),
        Some(n) => Some(Quantity::Bounded(NonZeroU64::new(n).unwrap())),
    }
}

// We need to check that the client passes in a tls cert pem string, hexadecimal macaroon,
// and address, so they can connect to LNDK's gRPC server.
fn check_auth_metadata(metadata: &MetadataMap) -> Result<String, Status> {
    let macaroon = match metadata.get("macaroon") {
        Some(macaroon_hex) => macaroon_hex
            .to_str()
            .map_err(|e| AuthError::InvalidMacaroon(e.to_string()).to_status())?
            .to_string(),
        _ => return Err(AuthError::MissingMacaroon.to_status()),
    };

    Ok(macaroon)
}

#[derive(Debug)]
pub enum AuthError {
    InvalidMacaroon(String),
    MissingMacaroon,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidMacaroon(e) => write!(f, "Invalid macaroon string provided: {e}"),
            AuthError::MissingMacaroon => write!(
                f,
                "No LND macaroon provided: Make sure to provide macaroon in request metadata"
            ),
        }
    }
}

impl std::error::Error for AuthError {}

impl AuthError {
    pub fn code(&self) -> &'static str {
        match self {
            AuthError::InvalidMacaroon(_) => "INVALID_MACAROON",
            AuthError::MissingMacaroon => "MISSING_MACAROON",
        }
    }

    pub fn grpc_code(&self) -> tonic::Code {
        match self {
            AuthError::InvalidMacaroon(_) => tonic::Code::InvalidArgument,
            AuthError::MissingMacaroon => tonic::Code::Unauthenticated,
        }
    }

    pub fn to_status(self) -> Status {
        let error_code = self.code();
        let grpc_code = self.grpc_code();
        let human_message = self.to_string();

        let error_info = format!(r#"{{"reason": "{}", "domain": "lndk"}}"#, error_code);

        Status::with_details(grpc_code, human_message, error_info.into())
    }
}

fn create_fee_limit(
    fee_limit: Option<u32>,
    fee_limit_percent: Option<u32>,
) -> Option<tonic_lnd::lnrpc::FeeLimit> {
    match (fee_limit, fee_limit_percent) {
        (Some(fixed), None) => Some(tonic_lnd::lnrpc::FeeLimit {
            limit: Some(tonic_lnd::lnrpc::fee_limit::Limit::FixedMsat(fixed as i64)),
        }),
        (None, Some(percent)) => Some(tonic_lnd::lnrpc::FeeLimit {
            limit: Some(tonic_lnd::lnrpc::fee_limit::Limit::Percent(percent as i64)),
        }),
        _ => None,
    }
}

fn generate_bolt12_invoice_contents(invoice: &Bolt12Invoice) -> lndkrpc::Bolt12InvoiceContents {
    Bolt12InvoiceContents {
        chain: invoice.chain().to_string(),
        quantity: invoice.quantity(),
        amount_msats: invoice.amount_msats(),
        description: invoice
            .description()
            .map(|description| description.to_string()),
        payment_hash: Some(PaymentHash {
            hash: invoice.payment_hash().encode(),
        }),
        created_at: invoice.created_at().as_secs() as i64,
        relative_expiry: invoice.relative_expiry().as_secs(),
        node_id: Some(convert_public_key(&invoice.signing_pubkey())),
        signature: invoice.signature().to_string(),
        payment_paths: extract_payment_paths(invoice),
        features: convert_invoice_features(invoice.invoice_features().clone()),
        payer_note: invoice
            .payer_note()
            .map(|payer_note| payer_note.to_string()),
    }
}

fn encode_invoice_as_hex(invoice: &Bolt12Invoice) -> Result<String, OfferError> {
    let mut buffer = Vec::new();
    invoice
        .write(&mut buffer)
        .map_err(OfferError::EncodeInvoiceFailure)?;
    Ok(hex::encode(buffer))
}

fn extract_payment_paths(invoice: &Bolt12Invoice) -> Vec<PaymentPaths> {
    invoice
        .payment_paths()
        .iter()
        .map(|path| PaymentPaths {
            blinded_pay_info: Some(convert_blinded_pay_info(&path.payinfo)),
            blinded_path: Some(convert_blinded_path(path)),
        })
        .collect()
}

fn convert_public_key(native_pub_key: &PublicKey) -> lndkrpc::PublicKey {
    let pub_key_bytes = native_pub_key.encode();
    lndkrpc::PublicKey { key: pub_key_bytes }
}

fn convert_invoice_features(_features: impl std::fmt::Debug) -> Vec<i32> {
    vec![FeatureBit::MppOpt as i32]
}

fn convert_blinded_pay_info(
    native_info: &lightning::blinded_path::payment::BlindedPayInfo,
) -> lndkrpc::BlindedPayInfo {
    lndkrpc::BlindedPayInfo {
        fee_base_msat: native_info.fee_base_msat,
        fee_proportional_millionths: native_info.fee_proportional_millionths,
        cltv_expiry_delta: native_info.cltv_expiry_delta as u32,
        htlc_minimum_msat: native_info.htlc_minimum_msat,
        htlc_maximum_msat: native_info.htlc_maximum_msat,
        ..Default::default()
    }
}

fn convert_blinded_path(native_info: &BlindedPaymentPath) -> lndkrpc::BlindedPath {
    let introduction_node = match native_info.introduction_node() {
        IntroductionNode::NodeId(pubkey) => lndkrpc::IntroductionNode {
            node_id: Some(convert_public_key(pubkey)),
            directed_short_channel_id: None,
        },
        IntroductionNode::DirectedShortChannelId(direction, scid) => {
            let rpc_direction = match direction {
                Direction::NodeOne => lndkrpc::Direction::NodeOne,
                Direction::NodeTwo => lndkrpc::Direction::NodeTwo,
            };

            lndkrpc::IntroductionNode {
                node_id: None,
                directed_short_channel_id: Some(lndkrpc::DirectedShortChannelId {
                    direction: rpc_direction.into(),
                    scid: *scid,
                }),
            }
        }
    };

    lndkrpc::BlindedPath {
        introduction_node: Some(introduction_node),
        blinding_point: Some(convert_public_key(&native_info.blinding_point())),
        blinded_hops: native_info
            .blinded_hops()
            .iter()
            .map(|hop| lndkrpc::BlindedHop {
                blinded_node_id: Some(convert_public_key(&hop.blinded_node_id)),
                encrypted_payload: hop.encrypted_payload.clone(),
            })
            .collect(),
    }
}
