use bitcoin::key::Secp256k1;
use bitcoin::Network;
use futures::executor::block_on;
use lightning::blinded_path::message::{BlindedMessagePath, MessageContext, OffersContext};
use lightning::blinded_path::payment::BlindedPaymentPath;
use lightning::blinded_path::{Direction, IntroductionNode};
use lightning::ln::channelmanager::{PaymentId, Verification};
use lightning::ln::inbound_payment::ExpandedKey;
use lightning::offers::invoice::{Bolt12Invoice, DerivedSigningPubkey, InvoiceBuilder};
use lightning::offers::invoice_error::InvoiceError;
use lightning::offers::invoice_request::InvoiceRequest;
use lightning::offers::nonce::Nonce;
use lightning::offers::offer::{Offer, Quantity};
use lightning::onion_message::messenger::{
    Destination, MessageSendInstructions, Responder, ResponseInstruction,
};
use lightning::onion_message::offers::{OffersMessage, OffersMessageHandler};
use lightning::sign::EntropySource;
use log::{debug, error, info};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tonic_lnd::lnrpc::{ChanInfoRequest, Payment};
use tonic_lnd::Client;

use super::requests::{
    create_invoice_info_from_request, create_invoice_request, create_offer, send_invoice_request,
    LndkBolt12InvoiceInfo,
};
use super::OfferError;
use crate::offers::requests::{send_payment, track_payment};
use crate::onion_messenger::MessengerUtilities;

pub const DEFAULT_RESPONSE_INVOICE_TIMEOUT: u32 = 15;

pub(crate) enum PaymentState {
    InvoiceRequestCreated,
    // InvoiceRequestSent,
    InvoiceReceived,
    PaymentDispatched,
    // Paid,
}
pub(crate) struct PaymentInfo {
    state: PaymentState,
    invoice: Option<Bolt12Invoice>,
}
pub struct OfferHandler {
    // active_payments holds a list of payments we're currently attempting to make. When we create
    // a new invoice request for a payment, we set a PaymentId in its metadata, which we also store
    // here. Then we wait until we receive an invoice with the same PaymentId.
    active_payments: Mutex<HashMap<PaymentId, PaymentInfo>>,
    pending_messages: Mutex<Vec<(OffersMessage, MessageSendInstructions)>>,
    pub messenger_utils: MessengerUtilities,
    expanded_key: ExpandedKey,
    /// The amount of time in seconds that we will wait for the offer creator to respond with
    /// an invoice. If not provided, we will use the default value of 15 seconds.
    pub response_invoice_timeout: u32,
    client: Option<Client>,
}

#[derive(Clone)]
pub struct PayOfferParams {
    pub offer: Offer,
    pub amount: Option<u64>,
    pub payer_note: Option<String>,
    pub network: Network,
    pub client: Client,
    /// The destination the offer creator provided, which we will use to send the invoice request.
    pub destination: Destination,
    /// The path we will send back to the offer creator, so it knows where to send back the
    /// invoice.
    pub reply_path: Option<BlindedMessagePath>,
    /// The amount of time in seconds that we will wait for the offer creator to respond with
    /// an invoice. If not provided, we will use the default value of 15 seconds.
    pub response_invoice_timeout: Option<u32>,
}

pub struct SendPaymentParams {
    pub path: BlindedPaymentPath,
    pub cltv_expiry_delta: u16,
    pub fee_base_msat: u32,
    pub fee_ppm: u32,
    pub payment_hash: [u8; 32],
    pub msats: u64,
    pub payment_id: PaymentId,
}

pub struct CreateOfferParams {
    /// LND tonic client used to query information from the node.
    pub client: Client,
    /// The amount of the offer in millisatoshis.
    pub amount_msats: u64,
    /// The chain the offer is valid on.
    pub chain: Network,
    /// Optional description of the offer. If not provided, the offer will have description "".
    pub description: Option<String>,
    /// Optional issuer of the offer. If not provided, the offer will have issuer None.
    pub issuer: Option<String>,
    /// Optional quantity of the offer. If not provided, the offer will have quantity
    /// Quantity::One.
    pub quantity: Option<Quantity>,
    /// Optional relative expiry of the offer since creation.
    /// If not provided, the offer will have expiry None, will never expire.
    pub expiry: Option<Duration>,
}

impl OfferHandler {
    pub fn new(
        response_invoice_timeout: Option<u32>,
        seed: Option<[u8; 32]>,
        client: Option<Client>,
    ) -> Self {
        let messenger_utils = MessengerUtilities::default();
        let random_bytes = match seed {
            Some(seed) => seed,
            None => messenger_utils.get_secure_random_bytes(),
        };
        let expanded_key = ExpandedKey::new(random_bytes);
        let response_invoice_timeout =
            response_invoice_timeout.unwrap_or(DEFAULT_RESPONSE_INVOICE_TIMEOUT);

        OfferHandler {
            active_payments: Mutex::new(HashMap::new()),
            pending_messages: Mutex::new(Vec::new()),
            messenger_utils,
            expanded_key,
            response_invoice_timeout,
            client,
        }
    }

    /// Adds an offer to be paid with the amount specified. May only be called once for a single
    /// offer.
    pub async fn pay_offer(&self, cfg: PayOfferParams) -> Result<Payment, OfferError> {
        let client_clone = cfg.client.clone();
        let (invoice, validated_amount, payment_id) = self.get_invoice(cfg).await?;

        self.pay_invoice(client_clone, validated_amount, &invoice, payment_id)
            .await
    }

    /// Sends an invoice request and waits for an invoice to be sent back to us.
    /// Reminder that if this method returns an error after create_invoice_request is called, we
    /// *must* remove the payment_id from self.active_payments.
    pub async fn get_invoice(
        &self,
        cfg: PayOfferParams,
    ) -> Result<(Bolt12Invoice, u64, PaymentId), OfferError> {
        let (invoice_request, payment_id, validated_amount, offer_context) =
            create_invoice_request(
                cfg.offer.clone(),
                cfg.network,
                &self.messenger_utils,
                self.expanded_key,
                cfg.amount,
                cfg.payer_note,
            )
            .await?;

        {
            let mut active_payments = self.active_payments.lock().unwrap();
            match active_payments.entry(payment_id) {
                Entry::Occupied(_) => return Err(OfferError::AlreadyProcessing(payment_id)),
                Entry::Vacant(v) => {
                    v.insert(PaymentInfo {
                        state: PaymentState::InvoiceRequestCreated,
                        invoice: None,
                    });
                }
            };
        }

        let (contents, send_instructions) = send_invoice_request(
            cfg.destination.clone(),
            cfg.client.clone(),
            invoice_request,
            offer_context,
            &self.messenger_utils,
        )
        .await
        .inspect_err(|_| {
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments.remove(&payment_id);
        })?;

        {
            let mut pending_messages = self.pending_messages.lock().unwrap();
            pending_messages.push((contents, send_instructions));
            std::mem::drop(pending_messages);
        }

        let cfg_timeout = cfg
            .response_invoice_timeout
            .unwrap_or(self.response_invoice_timeout);

        let invoice = match timeout(
            Duration::from_secs(cfg_timeout as u64),
            self.wait_for_invoice(payment_id),
        )
        .await
        {
            Ok(invoice) => invoice,
            Err(_) => {
                error!("Did not receive invoice in {cfg_timeout} seconds.");
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
                return Err(OfferError::InvoiceTimeout(cfg_timeout));
            }
        };
        {
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments
                .entry(payment_id)
                .and_modify(|entry| entry.state = PaymentState::InvoiceReceived);
        }

        Ok((invoice, validated_amount, payment_id))
    }

    /// Sends an invoice request and waits for an invoice to be sent back to us.
    /// Reminder that if this method returns an error after create_invoice_request is called, we
    /// *must* remove the payment_id from self.active_payments.
    pub(crate) async fn pay_invoice(
        &self,
        client: Client,
        amount: u64,
        invoice: &Bolt12Invoice,
        payment_id: PaymentId,
    ) -> Result<Payment, OfferError> {
        let payment_hash = invoice.payment_hash();
        let payment_path = &invoice.payment_paths()[0];

        let payment_hash = payment_hash.0;
        let params = SendPaymentParams {
            path: payment_path.clone(),
            cltv_expiry_delta: payment_path.payinfo.cltv_expiry_delta,
            fee_base_msat: payment_path.payinfo.fee_base_msat,
            fee_ppm: payment_path.payinfo.fee_proportional_millionths,
            payment_hash,
            msats: amount,
            payment_id,
        };

        let intro_node_id = match params.path.introduction_node() {
            IntroductionNode::NodeId(node_id) => Some(node_id.to_string()),
            IntroductionNode::DirectedShortChannelId(direction, scid) => {
                let get_chan_info_request = ChanInfoRequest {
                    chan_id: *scid,
                    chan_point: "".to_string(),
                };
                let chan_info = client
                    .clone()
                    .lightning_read_only()
                    .get_chan_info(get_chan_info_request)
                    .await
                    .map_err(OfferError::GetChannelInfo)?
                    .into_inner();
                match direction {
                    Direction::NodeOne => Some(chan_info.node1_pub),
                    Direction::NodeTwo => Some(chan_info.node2_pub),
                }
            }
        };
        debug!(
            "Attempting to pay invoice with introduction node {:?}",
            intro_node_id
        );

        send_payment(client.clone(), params)
            .await
            .inspect_err(|_| {
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
            })?;

        {
            let mut active_payments = self.active_payments.lock().unwrap();
            active_payments
                .entry(payment_id)
                .and_modify(|entry| entry.state = PaymentState::PaymentDispatched);
        }

        // We'll track the payment until it settles.
        track_payment(client, payment_hash)
            .await
            .inspect(|_| {
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
            })
            .inspect_err(|_| {
                let mut active_payments = self.active_payments.lock().unwrap();
                active_payments.remove(&payment_id);
            })
    }

    /// wait_for_invoice waits for the offer creator to respond with an invoice.
    async fn wait_for_invoice(&self, payment_id: PaymentId) -> Bolt12Invoice {
        loop {
            {
                let active_payments = self.active_payments.lock().unwrap();
                if let Some(pay_info) = active_payments.get(&payment_id) {
                    if let Some(invoice) = pay_info.invoice.clone() {
                        return invoice;
                    }
                };
            }
            sleep(Duration::from_secs(2)).await;
        }
    }

    pub(crate) fn remove_active_payment(&self, payment_id: PaymentId) {
        let mut active_payments = self.active_payments.lock().unwrap();
        active_payments.remove(&payment_id);
    }

    pub async fn create_offer(&self, params: CreateOfferParams) -> Result<Offer, OfferError> {
        create_offer(
            params.client.clone(),
            params.amount_msats,
            params.chain,
            params.description,
            params.issuer,
            params.quantity,
            params.expiry,
            &self.messenger_utils,
            &self.expanded_key,
        )
        .await
    }

    pub async fn create_invoice(
        &self,
        client: Client,
        invoice_request: InvoiceRequest,
    ) -> Result<LndkBolt12InvoiceInfo, OfferError> {
        create_invoice_info_from_request(client, invoice_request).await
    }
}

impl Default for OfferHandler {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

impl OffersMessageHandler for OfferHandler {
    fn handle_message(
        &self,
        message: OffersMessage,
        context: Option<OffersContext>,
        responder: Option<Responder>,
    ) -> Option<(OffersMessage, ResponseInstruction)> {
        match message {
            OffersMessage::InvoiceRequest(invoice_request) => {
                let responder = match responder {
                    Some(responder) => responder,
                    None => return None,
                };
                let offer_context = match context {
                    Some(context) => context,
                    None => return None,
                };

                let nonce = match offer_context {
                    OffersContext::InvoiceRequest { nonce } => nonce,
                    _ => {
                        return None;
                    }
                };

                let secp_ctx = &Secp256k1::new();

                // Clone invoice_request before verification since it consumes the value.
                // TODO: create_invoice should use VerifiedInvoiceRequest instead of InvoiceRequest.
                let invoice_request_clone = invoice_request.clone();
                let verfied_invoice = match invoice_request.verify_using_recipient_data(
                    nonce,
                    &self.expanded_key,
                    secp_ctx,
                ) {
                    Ok(invoice) => invoice,
                    Err(_) => return None,
                };

                let client = match self.client {
                    Some(_) => self.client.clone().unwrap(),
                    None => {
                        error!("No client provided to create invoice");
                        return None;
                    }
                };
                log::trace!("Creating invoice");
                let invoice_info =
                    match block_on(self.create_invoice(client, invoice_request_clone)) {
                        Ok(invoice) => invoice,
                        Err(e) => {
                            error!("Error creating invoice: {e}");
                            return None;
                        }
                    };
                log::trace!("Invoice created: {:?}", invoice_info);

                let invoice_builder = match verfied_invoice.respond_using_derived_keys(
                    invoice_info.payment_paths,
                    invoice_info.payment_hash,
                ) {
                    Ok(invoice_builder) => invoice_builder,
                    Err(e) => {
                        error!("Error building invoice: {:?}", e);
                        return None;
                    }
                };

                // Lnd doesn't support MPP in blinded paths, so we need to build the invoice without it.
                let invoice_result = InvoiceBuilder::<DerivedSigningPubkey>::from(invoice_builder)
                    .build_and_sign(secp_ctx)
                    .map_err(InvoiceError::from);

                match invoice_result {
                    Ok(invoice) => {
                        let nonce = Nonce::from_entropy_source(&*self.messenger_utils);
                        let hmac = invoice_info
                            .payment_hash
                            .hmac_for_offer_payment(nonce, &self.expanded_key);
                        let context = MessageContext::Offers(OffersContext::InboundPayment {
                            payment_hash: invoice_info.payment_hash,
                            nonce,
                            hmac,
                        });
                        log::trace!("Responding with invoice");
                        Some((
                            OffersMessage::Invoice(invoice),
                            responder.respond_with_reply_path(context),
                        ))
                    }
                    Err(error) => {
                        log::error!("Error building invoice: {:?}", error);
                        Some((
                            OffersMessage::InvoiceError(error.into()),
                            responder.respond(),
                        ))
                    }
                }
            }
            OffersMessage::Invoice(invoice) => {
                info!("Received an invoice: {invoice:?}");
                let secp_ctx = &Secp256k1::new();
                let offer_context = context.unwrap();
                let (payment_id, nonce) = match offer_context {
                    OffersContext::OutboundPayment {
                        nonce, payment_id, ..
                    } => (payment_id, nonce),
                    _ => {
                        error!("Received an invoice request for a payment id that we don't recognize or already paid. We will ignore the invoice.");
                        return None;
                    }
                };
                match invoice.verify_using_payer_data(
                    payment_id,
                    nonce,
                    &self.expanded_key,
                    secp_ctx,
                ) {
                    Ok(payment_id) => {
                        info!("Successfully verified invoice for payment_id {payment_id}");
                        let mut active_payments = self.active_payments.lock().unwrap();
                        match active_payments.get_mut(&payment_id) {
                            Some(pay_info) => match pay_info.invoice {
                                Some(_) => {
                                    error!("We already received an invoice with this payment id.")
                                }
                                None => {
                                    pay_info.state = PaymentState::InvoiceReceived;
                                    pay_info.invoice = Some(invoice.clone());
                                }
                            },
                            None => {
                                error!("We received an invoice request for a payment id that we don't recognize or already paid: {payment_id:?}. We will ignore the invoice.");
                            }
                        }
                        None
                    }
                    Err(()) => {
                        error!("Invoice verification failed for invoice: {invoice:?}");
                        responder.map(|r| {
                            (
                                OffersMessage::InvoiceError(InvoiceError::from_string(
                                    String::from("invoice verification failure"),
                                )),
                                r.respond(),
                            )
                        })
                    }
                }
            }
            OffersMessage::InvoiceError(error) => {
                log::error!("Invoice error received: {}", error);
                None
            }
        }
    }

    fn release_pending_messages(&self) -> Vec<(OffersMessage, MessageSendInstructions)> {
        core::mem::take(&mut self.pending_messages.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {}
