use std::{error::Error, fmt::Display};

use lightning::{
    ln::channelmanager::PaymentId,
    offers::{merkle::SignError, parse::Bolt12SemanticError},
};
use tonic::{Code, Status};
use tonic_lnd::tonic::Status as TonicStatus;

mod client_impls;
pub mod handler;
mod lnd_requests;
mod parse;

pub(crate) use lnd_requests::connect_to_peer;
pub use lnd_requests::create_reply_path;
pub use parse::{decode, get_destination, validate_amount};

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
    DeriveKeyFailure(TonicStatus),
    /// User provided an invalid amount.
    InvalidAmount(String),
    /// Invalid currency contained in the offer.
    InvalidCurrency,
    /// Unable to connect to peer.
    PeerConnectError(TonicStatus),
    /// No node address.
    NodeAddressNotFound,
    /// Cannot list peers.
    ListPeersFailure(TonicStatus),
    /// Failure to build a reply path.
    BuildBlindedPathFailure,
    /// Unable to find or send to payment route.
    RouteFailure(TonicStatus),
    /// Failed to track payment.
    TrackFailure(TonicStatus),
    /// Failed to send payment.
    PaymentFailure,
    /// Failed to receive an invoice back from offer creator before the timeout.
    InvoiceTimeout(u32),
    /// Failed to find introduction node for blinded path.
    IntroductionNodeNotFound,
    /// Cannot fetch channel info.
    GetChannelInfo(TonicStatus),
    /// Failed to create offer.
    CreateOfferFailure(Bolt12SemanticError),
    /// Failed to create offer with expiry time given system clock.
    CreateOfferTimeFailure,
    /// Failed to add invoice.
    AddInvoiceFailure(TonicStatus),
    /// Failed to decode payment request.
    DecodePaymentRequestFailure(TonicStatus),
    /// Failed to parse payment hash.
    ParsePaymentHashFailure(String),
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
            OfferError::InvoiceTimeout(e) => write!(f, "Did not receive invoice in {e:?} seconds."),
            OfferError::IntroductionNodeNotFound => write!(f, "Could not find introduction node."),
            OfferError::GetChannelInfo(e) => write!(f, "Could not fetch channel info: {e:?}"),
            OfferError::CreateOfferFailure(e) => write!(f, "Could not create offer: {e:?}"),
            OfferError::CreateOfferTimeFailure => write!(
                f,
                "Could not create offer with expiry time given system clock"
            ),
            OfferError::AddInvoiceFailure(e) => {
                write!(f, "Could not add invoice to lnd node: {e:?}")
            }
            OfferError::DecodePaymentRequestFailure(e) => {
                write!(f, "Could not decode payment request: {e:?}")
            }
            OfferError::ParsePaymentHashFailure(e) => {
                write!(f, "Could not parse payment hash: {e:?}")
            }
        }
    }
}

impl Error for OfferError {}

pub fn map_offer_error_to_code(error: &OfferError) -> &'static str {
    match error {
        OfferError::CreateOfferFailure(_) => "CREATE_OFFER_FAILURE",
        OfferError::CreateOfferTimeFailure => "CREATE_OFFER_TIME_FAILURE",
        OfferError::AddInvoiceFailure(_) => "ADD_INVOICE_FAILURE",
        OfferError::DecodePaymentRequestFailure(_) => "DECODE_PAYMENT_REQUEST_FAILURE",
        OfferError::ParsePaymentHashFailure(_) => "PARSE_PAYMENT_HASH_FAILURE",
        OfferError::InvalidAmount(_) => "INVALID_AMOUNT",
        OfferError::InvalidCurrency => "INVALID_CURRENCY",
        OfferError::AlreadyProcessing(_) => "ALREADY_PROCESSING",
        OfferError::BuildUIRFailure(_) => "BUILD_UIR_FAILURE",
        OfferError::SignError(_) => "SIGN_ERROR",
        OfferError::DeriveKeyFailure(_) => "DERIVE_KEY_FAILURE",
        OfferError::PeerConnectError(_) => "PEER_CONNECT_ERROR",
        OfferError::NodeAddressNotFound => "NODE_ADDRESS_NOT_FOUND",
        OfferError::ListPeersFailure(_) => "LIST_PEERS_FAILURE",
        OfferError::BuildBlindedPathFailure => "BUILD_BLINDED_PATH_FAILURE",
        OfferError::RouteFailure(_) => "ROUTE_FAILURE",
        OfferError::TrackFailure(_) => "TRACK_FAILURE",
        OfferError::PaymentFailure => "PAYMENT_FAILURE",
        OfferError::InvoiceTimeout(_) => "INVOICE_TIMEOUT",
        OfferError::IntroductionNodeNotFound => "INTRODUCTION_NODE_NOT_FOUND",
        OfferError::GetChannelInfo(_) => "GET_CHANNEL_INFO",
    }
}

pub fn map_offer_error_to_grpc_code(error: &OfferError) -> Code {
    match error {
        OfferError::InvalidAmount(_) | OfferError::InvalidCurrency => Code::InvalidArgument,
        _ => Code::Internal,
    }
}

pub fn create_lndk_status(error: OfferError) -> Status {
    let error_code = map_offer_error_to_code(&error);
    let grpc_code = map_offer_error_to_grpc_code(&error);
    let human_message = error.to_string();

    let error_info = format!(r#"{{"reason": "{}", "domain": "lndk"}}"#, error_code);

    Status::with_details(grpc_code, human_message, error_info.into())
}
