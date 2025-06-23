use lightning::blinded_path::payment::BlindedPaymentPath;
use lightning::blinded_path::IntroductionNode;
use log::error;
use tonic::async_trait;
use tonic_lnd::lnrpc::{HtlcAttempt, Payment, QueryRoutesResponse, Route};
use tonic_lnd::routerrpc::TrackPaymentRequest;
use tonic_lnd::tonic::Status;
use tonic_lnd::{
    lnrpc::{ListPeersRequest, ListPeersResponse, NodeInfo},
    signrpc::{KeyLocator, SignMessageReq},
    Client,
};

use crate::lnd::{InvoicePayer, MessageSigner, PeerConnector};

use super::lnd_requests::get_node_id;
use super::OfferError;

#[async_trait]
impl PeerConnector for Client {
    async fn list_peers(&mut self) -> Result<ListPeersResponse, Status> {
        let list_req = ListPeersRequest::default();
        self.lightning()
            .list_peers(list_req)
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

        self.lightning()
            .connect_peer(connect_req.clone())
            .await
            .map(|_| ())
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

        self.lightning()
            .get_node_info(req)
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
                continue;
            }
        }

        Err(OfferError::PaymentFailure)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::lnd::{InvoicePayer, PeerConnector};

    use mockall::mock;
    use tonic::async_trait;
    use tonic_lnd::lnrpc::{HtlcAttempt, NodeInfo, Route};
    use tonic_lnd::tonic::Status;

    mock! {
        pub TestPeerConnector{}

         #[async_trait]
         impl PeerConnector for TestPeerConnector {
             async fn list_peers(&mut self) -> Result<tonic_lnd::lnrpc::ListPeersResponse, Status>;
             async fn get_node_info(&mut self, pub_key: String, include_channels: bool) -> Result<NodeInfo, Status>;
             async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), Status>;
         }
    }

    mock! {
        pub TestInvoicePayer{}

        #[async_trait]
        impl InvoicePayer for TestInvoicePayer{
            async fn query_routes(&mut self, path: BlindedPaymentPath, cltv_expiry_delta: u16, fee_base_msat: u32, fee_ppm: u32, msats: u64) -> Result<QueryRoutesResponse, Status>;
            async fn send_to_route(&mut self, payment_hash: [u8; 32], route: Route) -> Result<HtlcAttempt, Status>;
            async fn track_payment(&mut self, payment_hash: [u8; 32]) -> Result<Payment, OfferError>;
        }
    }
}
