mod common;
use lndk;

use bitcoin::secp256k1::{PublicKey, Secp256k1};
use bitcoin::Network;
use bitcoincore_rpc::bitcoin::Network as RpcNetwork;
use bitcoincore_rpc::RpcApi;
use ldk_sample::node_api::Node as LdkNode;
use lightning::blinded_path::BlindedPath;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::offers::offer::Quantity;
use lightning::onion_message::{DefaultMessageRouter, OnionMessenger};
use lndk::lnd::LndNodeSigner;
use lndk::lndk_offers::{request_invoice, wait_for_invoice};
use lndk::onion_messenger::{CustomMessenger, InvoiceHandler, MessengerUtilities};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::time::{sleep, timeout, Duration as TokioDuration};
use tokio::{join, select};

async fn wait_to_receive_onion_message(
    ldk1: LdkNode,
    ldk2: LdkNode,
    lnd_id: PublicKey,
) -> (LdkNode, LdkNode) {
    let data: Vec<u8> = vec![72, 101, 108, 108, 111];

    let res = ldk1
        .send_onion_message(vec![lnd_id, ldk2.get_node_info().0], 65, data)
        .await;
    assert!(res.is_ok());

    // We need to give lndk time to process the message and forward it to ldk2.
    let ldk2 = match timeout(TokioDuration::from_secs(100), check_for_message(ldk2)).await {
        Err(_) => panic!("ldk2 did not receive onion message before timeout"),
        Ok(ldk2) => ldk2,
    };

    return (ldk1, ldk2);
}

// Checks for either a custom onion message or an offer-specific message.
async fn check_for_message(ldk: LdkNode) -> LdkNode {
    loop {
        if ldk.onion_message_handler.messages.lock().unwrap().len() == 1 {
            return ldk;
        }
        sleep(TokioDuration::from_secs(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lndk_forwards_onion_message() {
    let test_name = "lndk_forwards_onion_message";
    let (_bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    // Here we'll produce a little path of two channels. Both ldk nodes are connected to lnd like so:
    //
    // ldk1 <-> lnd <-> ldk2
    //
    // Notice that ldk1 and ldk2 are not connected directly to each other.
    let (pubkey, addr) = ldk1.get_node_info();
    let (pubkey_2, addr_2) = ldk2.get_node_info();
    lnd.connect_to_peer(pubkey, addr).await;
    lnd.connect_to_peer(pubkey_2, addr_2).await;
    let lnd_info = lnd.get_info().await;

    // Now we'll spin up lndk. Even though ldk1 and ldk2 are not directly connected, we'll show that lndk
    // successfully helps lnd forward the onion message from ldk1 to ldk2.
    let lnd_cfg = lndk::lnd::LndCfg::new(
        lnd.address,
        PathBuf::from_str(&lnd.cert_path).unwrap(),
        PathBuf::from_str(&lnd.macaroon_path).unwrap(),
    );
    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg,
        log_dir: Some(
            lndk_dir
                .join(format!("lndk-logs.txt"))
                .to_str()
                .unwrap()
                .to_string(),
        ),
    };
    select! {
        val = lndk::run(lndk_cfg) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        // We wait for ldk2 to receive the onion message.
        (ldk1, ldk2) = wait_to_receive_onion_message(ldk1, ldk2, PublicKey::from_str(&lnd_info.identity_pubkey).unwrap()) => {
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// Here we test the beginning of the BOLT 12 offers flow. We show that lndk successfully builds an
// invoice_request and sends it.
async fn test_lndk_send_invoice_request() {
    let test_name = "lndk_send_invoice_request";
    let (_bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    // Here we'll produce a little network path:
    //
    // ldk1 <-> ldk2 <-> lnd
    //
    // ldk1 will be the offer creator, which will build a blinded route from ldk2 to ldk1.
    let (pubkey, _) = ldk1.get_node_info();
    let (pubkey_2, addr_2) = ldk2.get_node_info();

    ldk1.connect_to_peer(pubkey_2, addr_2).await.unwrap();
    lnd.connect_to_peer(pubkey_2, addr_2).await;
    let lnd_info = lnd.get_info().await;
    let lnd_pubkey = PublicKey::from_str(&lnd_info.identity_pubkey).unwrap();

    let path_pubkeys = vec![pubkey_2, pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            20_000,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    // Now we'll spin up lndk, which should forward the invoice request to ldk2.
    let lnd_cfg = lndk::lnd::LndCfg::new(
        lnd.address,
        PathBuf::from_str(&lnd.cert_path).unwrap(),
        PathBuf::from_str(&lnd.macaroon_path).unwrap(),
    );
    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg,
        log_dir: Some(
            lndk_dir
                .join(format!("lndk-logs.txt"))
                .to_str()
                .unwrap()
                .to_string(),
        ),
    };

    let mut signer_clone = lnd.client.clone().unwrap();
    let signer_client = signer_clone.signer();
    let node_signer = LndNodeSigner::new(lnd_pubkey.clone(), signer_client);
    let messenger_utils = MessengerUtilities::new();
    let mut invoice_handler = InvoiceHandler {
        invoices: Arc::new(Mutex::new(VecDeque::new())),
    };
    let onion_messenger = OnionMessenger::new(
        &messenger_utils,
        &node_signer,
        &messenger_utils,
        &DefaultMessageRouter {},
        &mut invoice_handler,
        IgnoringMessageHandler {},
    );

    let client = lnd.client.clone().unwrap();
    let mut client_clone = lnd.client.clone().unwrap();
    let custom_msg_client = CustomMessenger {
        client: client_clone.lightning().to_owned(),
    };
    let blinded_path = offer.paths()[0].clone();
    let secp_ctx = Secp256k1::new();
    let reply_path =
        BlindedPath::new_for_message(&[pubkey_2, lnd_pubkey], &messenger_utils, &secp_ctx).unwrap();
    select! {
        val = lndk::run(lndk_cfg) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        // Make sure lndk successfully sends the invoice_request.
        res = request_invoice(client, &onion_messenger, custom_msg_client, offer, pubkey_2, reply_path, blinded_path) => {
            assert!(res.is_ok());

            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that when the offer creator responds with an invoice, lndk is able to receive it.
async fn test_lndk_wait_for_invoice() {
    let test_name = "lndk_wait_for_invoice";
    let (bitcoind, mut lnd, ldk1, ldk2, _lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    // Here we'll produce a little network of channels:
    //
    // ldk1 <- ldk2 <- lnd
    //
    // ldk1 will be the offer creator, which will build a blinded route from ldk2 to ldk1.
    let (pubkey, addr) = ldk1.get_node_info();
    let (pubkey_2, addr_2) = ldk2.get_node_info();

    ldk1.connect_to_peer(pubkey_2, addr_2).await.unwrap();
    lnd.connect_to_peer(pubkey_2, addr_2).await;

    let lnd_info = lnd.get_info().await;
    let lnd_pubkey = PublicKey::from_str(&lnd_info.identity_pubkey).unwrap();

    // ldk1 won't send back the invoice unless we have channels in our network.
    let ldk2_fund_addr = ldk2.bitcoind_client.get_new_address().await;
    let lnd_fund_addr = lnd.new_address().await.address;

    // We need to convert funding addresses to the form that the bitcoincore_rpc library recognizes.
    let ldk2_addr_string = ldk2_fund_addr.to_string();
    let ldk2_addr = bitcoincore_rpc::bitcoin::Address::from_str(&ldk2_addr_string)
        .unwrap()
        .require_network(RpcNetwork::Regtest)
        .unwrap();
    let lnd_addr = bitcoincore_rpc::bitcoin::Address::from_str(&lnd_fund_addr)
        .unwrap()
        .require_network(RpcNetwork::Regtest)
        .unwrap();
    let lnd_network_addr = lnd
        .address
        .replace("localhost", "127.0.0.1")
        .replace("https://", "");

    // Fund both of these nodes so we can open channels.
    bitcoind
        .node
        .client
        .generate_to_address(1, &ldk2_addr)
        .unwrap();
    bitcoind
        .node
        .client
        .generate_to_address(1, &lnd_addr)
        .unwrap();

    // Make sure the above funds are confirmed.
    bitcoind
        .node
        .client
        .generate_to_address(100, &ldk2_addr)
        .unwrap();

    ldk2.open_channel(pubkey, addr, 200000, 0, true)
        .await
        .unwrap();
    ldk2.open_channel(
        lnd_pubkey,
        SocketAddr::from_str(&lnd_network_addr).unwrap(),
        200000,
        100000,
        false,
    )
    .await
    .unwrap();

    bitcoind
        .node
        .client
        .generate_to_address(100, &ldk2_addr)
        .unwrap();

    // TODO: Restarting the lnd node here isn't ideal. But for some reason after we call LDK's open_channel
    // API call, LND blocks and is unable to receive any API requests. This might be a bug in ldk-sample or
    // our ldk-sample implementation.
    lnd.handle.kill().expect("lnd couldn't be killed");
    lnd.start().await;
    lnd.connect_to_peer(pubkey_2, addr_2).await;

    let path_pubkeys = vec![pubkey_2, pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            20_000,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    let mut signer_clone = lnd.client.clone().unwrap();
    let signer_client = signer_clone.signer();
    let node_signer = LndNodeSigner::new(lnd_pubkey.clone(), signer_client);
    let messenger_utils = MessengerUtilities::new();
    let mut invoice_handler = InvoiceHandler {
        invoices: Arc::new(Mutex::new(VecDeque::new())),
    };
    let onion_messenger = OnionMessenger::new(
        &messenger_utils,
        &node_signer,
        &messenger_utils,
        &DefaultMessageRouter {},
        &mut invoice_handler,
        IgnoringMessageHandler {},
    );

    let client = lnd.client.clone().unwrap();
    let mut client_clone = lnd.client.clone().unwrap();
    let custom_msg_client = CustomMessenger {
        client: client_clone.lightning().to_owned(),
    };
    let blinded_path = offer.paths()[0].clone();
    let secp_ctx = Secp256k1::new();
    let reply_path =
        BlindedPath::new_for_message(&[pubkey_2, lnd_pubkey], &messenger_utils, &secp_ctx).unwrap();

    let (res1, res2) = join!(
        wait_for_invoice(client_clone.lightning(), &onion_messenger),
        request_invoice(
            client.clone(),
            &onion_messenger,
            custom_msg_client,
            offer,
            pubkey_2,
            reply_path,
            blinded_path
        )
    );
    res1.unwrap();
    res2.unwrap();

    // LND should have received the Bolt12Invoice.
    let invoices = invoice_handler.invoices.lock().unwrap();
    assert!(invoices.len() == 1);
}
