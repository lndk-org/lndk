use lightning::blinded_path::payment::BlindedPaymentPath;
use lightning::blinded_path::IntroductionNode;
use lightning::offers::invoice_request::InvoiceRequest;
use lightning::offers::offer::Amount;
use log::{error, trace};
use tonic::async_trait;
use tonic_lnd::lnrpc::{
    AddInvoiceResponse, FeeLimit, GetInfoRequest, GetInfoResponse, HtlcAttempt, Invoice, PayReq,
    PayReqString, Payment, QueryRoutesResponse, Route,
};
use tonic_lnd::routerrpc::TrackPaymentRequest;
use tonic_lnd::tonic::Status;
use tonic_lnd::LightningClient;
use tonic_lnd::{
    lnrpc::{
        ListChannelsRequest, ListChannelsResponse, ListPeersRequest, ListPeersResponse, NodeInfo,
    },
    signrpc::{KeyLocator, SignMessageReq},
    Client,
};

use crate::lnd::{Bolt12InvoiceCreator, InvoicePayer, MessageSigner, OfferCreator, PeerConnector};

use super::lnd_requests::get_node_id;
use super::OfferError;

#[async_trait]
impl PeerConnector for LightningClient {
    async fn list_peers(&mut self) -> Result<ListPeersResponse, Status> {
        let list_req = ListPeersRequest::default();
        self.list_peers(list_req)
            .await
            .map(|resp| resp.into_inner())
    }

    async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status> {
        let ln_addr = tonic_lnd::lnrpc::LightningAddress {
            pubkey: node_id,
            host: addr,
        };

        let connect_req = tonic_lnd::lnrpc::ConnectPeerRequest {
            addr: Some(ln_addr),
            timeout: 20,
            ..Default::default()
        };

        self.connect_peer(connect_req.clone()).await.map(|_| ())
    }

    async fn get_node_info(
        &mut self,
        pub_key: String,
        include_channels: bool,
    ) -> Result<NodeInfo, Status> {
        let req = tonic_lnd::lnrpc::NodeInfoRequest {
            pub_key,
            include_channels,
        };

        self.get_node_info(req).await.map(|resp| resp.into_inner())
    }

    async fn list_active_public_channels(&mut self) -> Result<ListChannelsResponse, Status> {
        let list_req = ListChannelsRequest {
            active_only: true,
            inactive_only: false,
            public_only: true,
            private_only: false,
            ..Default::default()
        };
        self.list_channels(list_req)
            .await
            .map(|resp| resp.into_inner())
    }
}

#[async_trait]
impl MessageSigner for Client {
    async fn sign_message(
        &mut self,
        msg: &[u8],
        key_loc: KeyLocator,
        double_hash: bool,
        schnorr_sig: bool,
    ) -> Result<Vec<u8>, ()> {
        let req = SignMessageReq {
            msg: msg.to_vec(),
            key_loc: Some(key_loc),
            double_hash,
            schnorr_sig,
            ..Default::default()
        };

        let resp = self
            .signer()
            .sign_message(req)
            .await
            .expect("Failed to sign message");
        let resp_inner = resp.into_inner();
        Ok(resp_inner.signature)
    }
}

#[async_trait]
impl InvoicePayer for Client {
    async fn query_routes(
        &mut self,
        path: BlindedPaymentPath,
        cltv_expiry_delta: u16,
        fee_base_msat: u32,
        fee_ppm: u32,
        msats: u64,
        fee_limit: Option<FeeLimit>,
    ) -> Result<QueryRoutesResponse, Status> {
        let mut blinded_hops = vec![];
        for hop in path.blinded_hops().iter() {
            let new_hop = tonic_lnd::lnrpc::BlindedHop {
                blinded_node: hop.blinded_node_id.serialize().to_vec(),
                encrypted_data: hop.clone().encrypted_payload,
            };
            blinded_hops.push(new_hop);
        }

        // We'll need to store the pubkey outside the match to fix lifetime issues
        let node_id_from_scid;
        let introduction_node = match path.introduction_node() {
            IntroductionNode::NodeId(pubkey) => pubkey,
            IntroductionNode::DirectedShortChannelId(direction, scid) => {
                node_id_from_scid =
                    get_node_id(self.clone(), *scid, *direction)
                        .await
                        .map_err(|e| {
                            error!("{e}");
                            Status::unknown("Could not get node id.")
                        })?;

                // Using the longer-lived reference
                &node_id_from_scid
            }
        };
        let blinded_path = Some(tonic_lnd::lnrpc::BlindedPath {
            introduction_node: introduction_node.serialize().to_vec(),
            blinding_point: path.blinding_point().serialize().to_vec(),
            blinded_hops,
        });

        let blinded_payment_paths = tonic_lnd::lnrpc::BlindedPaymentPath {
            blinded_path,
            total_cltv_delta: u32::from(cltv_expiry_delta) + 120,
            base_fee_msat: u64::from(fee_base_msat),
            proportional_fee_rate: fee_ppm,
            ..Default::default()
        };

        let query_req = tonic_lnd::lnrpc::QueryRoutesRequest {
            amt_msat: msats as i64,
            blinded_payment_paths: vec![blinded_payment_paths],
            fee_limit,
            ..Default::default()
        };

        let resp = self.lightning().query_routes(query_req).await?;
        Ok(resp.into_inner())
    }

    async fn send_to_route(
        &mut self,
        payment_hash: [u8; 32],
        route: Route,
    ) -> Result<HtlcAttempt, Status> {
        let send_req = tonic_lnd::routerrpc::SendToRouteRequest {
            payment_hash: payment_hash.to_vec(),
            route: Some(route),
            ..Default::default()
        };

        let resp = self.router().send_to_route_v2(send_req).await?;
        Ok(resp.into_inner())
    }

    async fn track_payment(&mut self, payment_hash: [u8; 32]) -> Result<Payment, OfferError> {
        let req = TrackPaymentRequest {
            payment_hash: payment_hash.to_vec(),
            no_inflight_updates: true,
        };

        let mut stream = self
            .router()
            .track_payment_v2(req)
            .await
            .map_err(OfferError::TrackFailure)?
            .into_inner();

        // Wait for a failed or successful payment.
        while let Some(payment) = stream.message().await.map_err(OfferError::TrackFailure)? {
            if payment.status() == tonic_lnd::lnrpc::payment::PaymentStatus::Succeeded {
                return Ok(payment);
            } else if payment.status() == tonic_lnd::lnrpc::payment::PaymentStatus::Failed {
                return Err(OfferError::PaymentFailure);
            } else {
                trace!(
                    "Payment with preimage {} has not settled.",
                    payment.payment_preimage,
                );
                continue;
            }
        }

        Err(OfferError::PaymentFailure)
    }
}

#[async_trait]
impl OfferCreator for LightningClient {
    async fn get_info(&mut self) -> Result<GetInfoResponse, Status> {
        let req = GetInfoRequest::default();
        self.get_info(req).await.map(|resp| resp.into_inner())
    }
}

#[async_trait]
impl Bolt12InvoiceCreator for Client {
    async fn add_invoice(
        &mut self,
        invoice_request: InvoiceRequest,
    ) -> Result<AddInvoiceResponse, Status> {
        let amount = match invoice_request.amount() {
            Some(Amount::Bitcoin { amount_msats }) => amount_msats as i64,
            _ => 0,
        };
        let description = match invoice_request.description() {
            Some(description) => description.to_string(),
            None => "".to_string(),
        };
        let req = Invoice {
            memo: description,
            value_msat: amount,
            // We use a 24 hour expiry for invoices so blinded path restrictions have a higher CLTV
            // expiry. This is a workaround for LDK default nodes that add a cltv offet
            // for privacy reasons.
            expiry: 60 * 60 * 24,
            is_blinded: true,
            ..Default::default()
        };
        let resp = self.lightning().add_invoice(req).await?;
        Ok(resp.into_inner())
    }

    async fn decode_payment_request(&mut self, payment_request: String) -> Result<PayReq, Status> {
        let req = PayReqString {
            pay_req: payment_request,
        };
        let resp = self.lightning().decode_pay_req(req).await?;
        Ok(resp.into_inner())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::lnd::{Bolt12InvoiceCreator, InvoicePayer, PeerConnector};
    use lightning::offers::invoice_request::InvoiceRequest;
    use mockall::mock;
    use tonic::async_trait;
    use tonic_lnd::lnrpc::{AddInvoiceResponse, PayReq};
    use tonic_lnd::lnrpc::{HtlcAttempt, NodeInfo, Route};
    use tonic_lnd::tonic::Status;

    mock! {
        pub TestPeerConnector{}

        impl Clone for TestPeerConnector {
            fn clone(&self) -> Self;
        }

         #[async_trait]
         impl PeerConnector for TestPeerConnector {
             async fn list_peers(&mut self) -> Result<tonic_lnd::lnrpc::ListPeersResponse, Status>;
             async fn get_node_info(&mut self, pub_key: String, include_channels: bool) -> Result<NodeInfo, Status>;
             async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
             async fn list_active_public_channels(&mut self) -> Result<ListChannelsResponse, Status>;
         }
    }

    mock! {
        pub TestInvoicePayer{}

        #[async_trait]
        impl InvoicePayer for TestInvoicePayer{
            async fn query_routes(&mut self, path: BlindedPaymentPath, cltv_expiry_delta: u16, fee_base_msat: u32, fee_ppm: u32, msats: u64, fee_limit: Option<FeeLimit>) -> Result<QueryRoutesResponse, Status>;
            async fn send_to_route(&mut self, payment_hash: [u8; 32], route: Route) -> Result<HtlcAttempt, Status>;
            async fn track_payment(&mut self, payment_hash: [u8; 32]) -> Result<Payment, OfferError>;
        }
    }

    mock! {
        pub TestOfferCreator{}

        #[async_trait]
        impl OfferCreator for TestOfferCreator {
            async fn get_info(&mut self) -> Result<GetInfoResponse, Status>;
        }

        #[async_trait]
        impl PeerConnector for TestOfferCreator {
            async fn list_peers(&mut self) -> Result<tonic_lnd::lnrpc::ListPeersResponse, Status>;
            async fn get_node_info(&mut self, pub_key: String, include_channels: bool) -> Result<NodeInfo, Status>;
            async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
            async fn list_active_public_channels(&mut self) -> Result<ListChannelsResponse, Status>;
        }
    }

    mock! {
        pub TestBolt12InvoiceCreator{}

        #[async_trait]
        impl Bolt12InvoiceCreator for TestBolt12InvoiceCreator {
            async fn add_invoice(&mut self, invoice_request: InvoiceRequest) -> Result<AddInvoiceResponse, Status>;
            async fn decode_payment_request(&mut self, payment_request: String) -> Result<PayReq, Status>;
        }
    }
}
