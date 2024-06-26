mod common;
use lndk;

use bitcoin::secp256k1::{PublicKey, Secp256k1};
use bitcoin::Network;
use bitcoincore_rpc::bitcoin::Network as RpcNetwork;
use bitcoincore_rpc::RpcApi;
use ldk_sample::node_api::Node as LdkNode;
use lightning::blinded_path::BlindedPath;
use lightning::offers::offer::Quantity;
use lightning::onion_message::messenger::Destination;
use lndk::lnd::validate_lnd_creds;
use lndk::onion_messenger::MessengerUtilities;
use lndk::{setup_logger, LifecycleSignals, OfferHandler, PayOfferParams};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::select;
use tokio::time::{sleep, timeout, Duration};

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
    let ldk2 = match timeout(Duration::from_secs(100), check_for_message(ldk2)).await {
        Err(_) => panic!("ldk2 did not receive onion message before timeout"),
        Ok(ldk2) => ldk2,
    };

    return (ldk1, ldk2);
}

async fn check_for_message(ldk: LdkNode) -> LdkNode {
    loop {
        if ldk.onion_message_handler.messages.lock().unwrap().len() == 1 {
            return ldk;
        }
        sleep(Duration::from_secs(2)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lndk_forwards_onion_message() {
    let test_name = "lndk_forwards_onion_message";
    let (_bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    // Here we'll produce a little path of two channels. Both ldk nodes are connected to lnd like
    // so:
    //
    // ldk1 <-> lnd <-> ldk2
    //
    // Notice that ldk1 and ldk2 are not connected directly to each other.
    let (pubkey, addr) = ldk1.get_node_info();
    let (pubkey_2, addr_2) = ldk2.get_node_info();
    lnd.connect_to_peer(pubkey, addr).await;
    lnd.connect_to_peer(pubkey_2, addr_2).await;
    let lnd_info = lnd.get_info().await;

    // Now we'll spin up lndk. Even though ldk1 and ldk2 are not directly connected, we'll show that
    // lndk successfully helps lnd forward the onion message from ldk1 to ldk2.
    let (shutdown, listener) = triggered::trigger();

    let creds = validate_lnd_creds(
        Some(PathBuf::from_str(&lnd.cert_path).unwrap()),
        None,
        Some(PathBuf::from_str(&lnd.macaroon_path).unwrap()),
        None,
    )
    .unwrap();
    let lnd_cfg = lndk::lnd::LndCfg::new(lnd.address, creds);

    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener,
    };
    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg,
        signals,
    };

    let log_dir = Some(
        lndk_dir
            .join(format!("lndk-logs.txt"))
            .to_str()
            .unwrap()
            .to_string(),
    );
    setup_logger(None, log_dir).unwrap();

    let handler = Arc::new(lndk::OfferHandler::new());
    let messenger = lndk::LndkOnionMessenger::new();
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        // We wait for ldk2 to receive the onion message.
        (ldk1, ldk2) = wait_to_receive_onion_message(ldk1, ldk2, PublicKey::from_str(&lnd_info.identity_pubkey).unwrap()) => {
            shutdown.trigger();
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
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    // Here we'll produce a little network. ldk1 will be the offer creator in this scenario. We'll
    // connect ldk1 and ldk2 with a channel so ldk1 can create an offer and ldk2 can be the
    // introduction node for the blinded path.
    //
    // Later on we'll disconnect lnd to ldk2 to make sure lnd can still auto-connect to the
    // introduction node.
    //
    // ldk1 <--- channel ---> ldk2 <--- peer connection ---> lnd
    //
    // ldk1 will be the offer creator, which will build a blinded route from ldk2 to ldk1.
    let (pubkey, addr) = ldk1.get_node_info();
    let (pubkey_2, addr_2) = ldk2.get_node_info();
    let lnd_info = lnd.get_info().await;
    let lnd_pubkey = PublicKey::from_str(&lnd_info.identity_pubkey).unwrap();

    ldk1.connect_to_peer(pubkey_2, addr_2).await.unwrap();
    lnd.connect_to_peer(pubkey_2, addr_2).await;

    let ldk2_fund_addr = ldk2.bitcoind_client.get_new_address().await;

    // We need to convert funding addresses to the form that the bitcoincore_rpc library recognizes.
    let ldk2_addr_string = ldk2_fund_addr.to_string();
    let ldk2_addr = bitcoind::bitcoincore_rpc::bitcoin::Address::from_str(&ldk2_addr_string)
        .unwrap()
        .require_network(RpcNetwork::Regtest)
        .unwrap();

    // Fund both of these nodes, open the channels, and synchronize the network.
    bitcoind
        .node
        .client
        .generate_to_address(6, &ldk2_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    ldk2.open_channel(pubkey, addr, 200000, 0, true)
        .await
        .unwrap();

    lnd.wait_for_graph_sync().await;

    bitcoind
        .node
        .client
        .generate_to_address(20, &ldk2_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

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
    let (shutdown, listener) = triggered::trigger();

    let creds = validate_lnd_creds(
        Some(PathBuf::from_str(&lnd.cert_path).unwrap()),
        None,
        Some(PathBuf::from_str(&lnd.macaroon_path).unwrap()),
        None,
    )
    .unwrap();
    let lnd_cfg = lndk::lnd::LndCfg::new(lnd.address.clone(), creds);

    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener,
    };

    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg.clone(),
        signals,
    };

    let mut client = lnd.client.clone().unwrap();
    let blinded_path = offer.paths()[0].clone();

    let messenger_utils = MessengerUtilities::new();
    let secp_ctx = Secp256k1::new();
    let reply_path =
        BlindedPath::new_for_message(&[pubkey_2, lnd_pubkey], &messenger_utils, &secp_ctx).unwrap();

    let mut stream = client
        .lightning()
        .subscribe_channel_graph(tonic_lnd::lnrpc::GraphTopologySubscription {})
        .await
        .unwrap()
        .into_inner();

    // Wait for ldk2's graph update to come through, otherwise when we try to auto-connect to
    // the introduction node later on, the address won't be available when we call the
    // describe_graph API method.
    'outer: while let Some(update) = stream.message().await.unwrap() {
        for node in update.node_updates.iter() {
            for node_addr in node.node_addresses.iter() {
                if node_addr.addr == addr_2.to_string() {
                    break 'outer;
                }
            }
        }
    }

    let log_dir = Some(
        lndk_dir
            .join(format!("lndk-logs.txt"))
            .to_str()
            .unwrap()
            .to_string(),
    );
    setup_logger(None, log_dir).unwrap();

    // Make sure lndk successfully sends the invoice_request.
    let handler = Arc::new(lndk::OfferHandler::new());
    let messenger = lndk::LndkOnionMessenger::new();
    let (invoice_request, _, _) = handler
        .create_invoice_request(
            client.clone(),
            offer.clone(),
            vec![],
            Network::Regtest,
            Some(20_000),
        )
        .await
        .unwrap();
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        // We wait for ldk2 to receive the onion message.
        res = handler.send_invoice_request(
            Destination::BlindedPath(blinded_path.clone()),
            client.clone(),
            offer.clone(),
            Some(reply_path.clone()),
            invoice_request
        ) => {
            assert!(res.is_ok());
        }
    }

    // Let's try again, but, make sure we can request the invoice when the LND node is not already
    // connected to the introduction node (LDK2).
    lnd.disconnect_peer(pubkey_2).await;
    lnd.wait_for_chain_sync().await;

    let (shutdown, listener) = triggered::trigger();
    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener,
    };

    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg,
        signals,
    };

    let log_dir = Some(
        lndk_dir
            .join(format!("lndk-logs.txt"))
            .to_str()
            .unwrap()
            .to_string(),
    );
    setup_logger(None, log_dir).unwrap();

    let handler = Arc::new(lndk::OfferHandler::new());
    let messenger = lndk::LndkOnionMessenger::new();
    let (invoice_request, _, _) = handler
        .create_invoice_request(
            client.clone(),
            offer.clone(),
            vec![],
            Network::Regtest,
            Some(20_000),
        )
        .await
        .unwrap();
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        // We wait for ldk2 to receive the onion message.
        res = handler.send_invoice_request(
            Destination::BlindedPath(blinded_path.clone()),
            client.clone(),
            offer.clone(),
            Some(reply_path.clone()),
            invoice_request
        ) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that we're able to fully pay an offer.
async fn test_lndk_pay_offer() {
    let test_name = "lndk_pay_offer";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, &mut lnd, &bitcoind).await;

    let path_pubkeys = vec![ldk2_pubkey, ldk1_pubkey];
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

    let (lndk_cfg, handler, messenger, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    let messenger_utils = MessengerUtilities::new();
    let client = lnd.client.clone().unwrap();
    let blinded_path = offer.paths()[0].clone();
    let secp_ctx = Secp256k1::new();
    let reply_path =
        BlindedPath::new_for_message(&[ldk2_pubkey, lnd_pubkey], &messenger_utils, &secp_ctx)
            .unwrap();

    let pay_cfg = PayOfferParams {
        offer,
        amount: Some(20_000),
        network: Network::Regtest,
        client: client.clone(),
        destination: Destination::BlindedPath(blinded_path.clone()),
        reply_path: Some(reply_path),
    };
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        // We wait for ldk2 to receive the onion message.
        res = handler.pay_offer(pay_cfg) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}
