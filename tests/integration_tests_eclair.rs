#![cfg(eclair_test)]

mod common;

use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use common::BitcoindNode;
use reqwest::Client as ReqwestClient;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::select;

struct EclairApi {
    client: ReqwestClient,
    base_url: String,
    password: String,
}

#[derive(Deserialize, Debug)]
struct GetInfoResponse {
    #[serde(rename = "nodeId")]
    node_id: String,
    #[serde(rename = "publicAddresses")]
    public_addresses: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct CreateOfferResponse {
    encoded: String,
}

impl EclairApi {
    fn new() -> Self {
        Self {
            client: ReqwestClient::new(),
            base_url: "http://localhost:8080".to_string(),
            password: "pass".to_string(),
        }
    }

    async fn call_direct<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let response = self
            .client
            .post(&url)
            .basic_auth("", Some(&self.password))
            .send()
            .await?
            .json::<T>()
            .await?;

        Ok(response)
    }

    async fn call_post<T: serde::de::DeserializeOwned>(
        &self,
        endpoint: &str,
        params: HashMap<String, String>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let url = format!("{}/{}", self.base_url, endpoint);
        let response = self
            .client
            .post(&url)
            .basic_auth("", Some(&self.password))
            .form(&params)
            .send()
            .await?
            .json::<T>()
            .await?;

        Ok(response)
    }

    async fn get_info(&self) -> Result<GetInfoResponse, Box<dyn std::error::Error>> {
        self.call_direct("getinfo").await
    }

    async fn create_offer(
        &self,
        description: &str,
        introduction_node: Option<String>,
        amount_msat: Option<u64>,
    ) -> Result<CreateOfferResponse, Box<dyn std::error::Error>> {
        let mut params = HashMap::new();
        params.insert("description".to_string(), description.to_string());

        if let Some(node) = introduction_node {
            params.insert("blindedPathsFirstNodeId".to_string(), node);
        }

        if let Some(amount) = amount_msat {
            params.insert("amountMsat".to_string(), amount.to_string());
        }

        self.call_post::<CreateOfferResponse>("createoffer", params)
            .await
    }
}

fn parse_socket_addr(addr_str: &str) -> Option<SocketAddr> {
    let (host, port_str) = match addr_str {
        s if s.starts_with("https://") => extract_host_port(s.strip_prefix("https://")?),
        s if s.starts_with("http://") => extract_host_port(s.strip_prefix("http://")?),
        s if s.starts_with("ipv4://") => extract_host_port(s.strip_prefix("ipv4://")?),
        s if s.starts_with("ipv6://") => extract_ipv6_host_port(s.strip_prefix("ipv6://")?),
        s => extract_host_port(s),
    }?;

    let port = port_str.parse::<u16>().ok()?;

    let host_str = if host == "localhost" {
        "127.0.0.1"
    } else {
        host
    };

    format!("{}:{}", host_str, port).parse().ok()
}

fn extract_host_port(stripped: &str) -> Option<(&str, &str)> {
    let port_index = stripped.rfind(':')?;
    let host = &stripped[..port_index];
    let port_str = &stripped[port_index + 1..];
    Some((host, port_str))
}

fn extract_ipv6_host_port(stripped: &str) -> Option<(&str, &str)> {
    if stripped.starts_with('[') {
        let end_bracket = stripped.rfind(']')?;
        if stripped.len() > end_bracket + 2 && stripped.as_bytes()[end_bracket + 1] == b':' {
            let host = &stripped[1..end_bracket]; // Remove brackets
            let port_str = &stripped[end_bracket + 2..];
            return Some((host, port_str));
        }
    }
    None
}

fn connect_bitcoind_nodes(bitcoind: &BitcoindNode) {
    bitcoind
        .node
        .client
        .call::<()>(
            "addnode",
            &[
                serde_json::to_value("127.0.0.1:18444").unwrap(),
                serde_json::to_value("add").unwrap(),
            ],
        )
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lndk_pay_eclair_offer() {
    let test_name = "lndk_pay_eclair_offer";
    let (bitcoind, mut lnd, ldk1, _, lndk_dir) = common::setup_test_infrastructure(test_name).await;

    connect_bitcoind_nodes(&bitcoind);

    // --- Eclair Setup ---
    let eclair_api = EclairApi::new();

    // Get Eclair info (Node ID, Address)
    let eclair_info = eclair_api
        .get_info()
        .await
        .expect("Failed to get Eclair info");
    let eclair_node_id = PublicKey::from_str(&eclair_info.node_id).expect("Invalid Eclair node ID");

    // Get the first public address from Eclair's response
    let eclair_addr_str = eclair_info
        .public_addresses
        .first()
        .expect("Eclair has no public addresses");

    let eclair_socket_addr =
        parse_socket_addr(eclair_addr_str).expect("Could not parse Eclair socket address");

    // --- Network Setup ---
    let (ldk1_pubkey, ldk1_addr) = ldk1.get_node_info(); // Use get_node_info
    let lnd_info = lnd.get_info().await;
    let lnd_pubkey = PublicKey::from_str(&lnd_info.identity_pubkey).unwrap();

    lnd.connect_to_peer(ldk1_pubkey, ldk1_addr).await;
    // Connect LND to Eclair to speed up Graph Sync
    lnd.connect_to_peer(eclair_node_id, eclair_socket_addr)
        .await;
    // Fund nodes
    let ldk1_fund_addr = ldk1.bitcoind_client.get_new_address().await;
    let ldk1_addr_str = ldk1_fund_addr.to_string();
    let ldk1_addr_rpc = bitcoincore_rpc::bitcoin::Address::from_str(&ldk1_addr_str)
        .unwrap()
        .require_network(bitcoincore_rpc::bitcoin::Network::Regtest)
        .unwrap();
    let lnd_network_addr = lnd
        .address
        .replace("localhost", "127.0.0.1")
        .replace("https://", "");

    bitcoind
        .node
        .client
        .generate_to_address(25, &ldk1_addr_rpc)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    // Open channel LND <-> LDK1
    ldk1.open_channel(
        lnd_pubkey,
        SocketAddr::from_str(&lnd_network_addr).unwrap(),
        15_000_000,
        10_000_000_000,
        true,
    )
    .await
    .unwrap();

    lnd.wait_for_graph_sync().await;

    ldk1.connect_to_peer(eclair_node_id, eclair_socket_addr)
        .await
        .expect("LDK1 -> Eclair connection failed");

    // Open channel LDK1 <-> Eclair
    ldk1.open_channel(
        eclair_node_id,
        eclair_socket_addr,
        15_000_000,
        10_000_000_000,
        true,
    )
    .await
    .expect("Failed to open channel LDK1 -> Eclair");

    lnd.wait_for_graph_sync().await;

    bitcoind
        .node
        .client
        .generate_to_address(75, &ldk1_addr_rpc)
        .unwrap();

    println!("Mined 75 blocks");

    lnd.wait_for_chain_sync().await;

    println!("LND synced to chain and graph");
    let introduction_node = ldk1_pubkey.to_string();

    // --- Offer Creation (Eclair) ---
    let offer_desc = format!("test-offer-{}", test_name);
    let offer_amount_msat = 50_000;
    let offer_str = eclair_api
        .create_offer(
            &offer_desc,
            Some(introduction_node),
            Some(offer_amount_msat),
        )
        .await
        .expect("Failed to create offer in Eclair");

    println!("Eclair offer created");
    // --- LNDK Setup ---
    let (lndk_cfg, handler, messenger, shutdown) = common::setup_lndk(
        &lnd.cert_path,
        &lnd.macaroon_path,
        lnd.address.clone(),
        lndk_dir,
    )
    .await;
    let offer = lightning::offers::offer::Offer::from_str(&offer_str.encoded)
        .expect("Failed to parse offer string");

    println!("Offer parsed");
    // --- Offer Payment (LNDK) ---
    let destination = lndk::lndk_offers::get_destination(&offer)
        .await
        .expect("Could not get destination from offer");

    println!("Destination got");

    lnd.wait_for_check_node_has_address(eclair_node_id).await;
    println!("LND checked node has address");

    let reply_path = match handler
        .create_reply_path(lnd.client.clone().unwrap(), lnd_pubkey)
        .await
    {
        Ok(reply_path) => Some(reply_path),
        Err(e) => panic!("Failed to create reply path: {}", e),
    };

    let pay_cfg = lndk::PayOfferParams {
        offer,
        amount: Some(offer_amount_msat),
        payer_note: Some("Paying Eclair from LNDK test".to_string()),
        network: Network::Regtest,
        client: lnd.client.clone().unwrap(),
        destination,
        reply_path,
        response_invoice_timeout: None,
    };

    // Run LNDK and pay the offer
    select! {
        val = messenger.run(lndk_cfg.clone(), Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = handler.pay_offer(pay_cfg.clone()) => {
            assert!(res.is_ok(), "Paying the Eclair offer failed: {:?}", res.err());
            println!("Successfully paid Eclair offer!");

            shutdown.trigger();
            ldk1.stop().await;
        }
    };
}
