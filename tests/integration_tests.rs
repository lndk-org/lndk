mod common;
use futures::future::try_join_all;
use lndk;

use bitcoin::secp256k1::{PublicKey, Secp256k1};
use bitcoin::Network;
use bitcoincore_rpc::bitcoin::Network as RpcNetwork;
use bitcoincore_rpc::RpcApi;
use bitcoind::get_available_port;
use common::LndkCli;
use ldk_sample::node_api::Node as LdkNode;
use lightning::blinded_path::{BlindedPath, IntroductionNode};
use lightning::offers::offer::Quantity;
use lightning::onion_message::messenger::Destination;
use lndk::lnd::validate_lnd_creds;
use lndk::onion_messenger::MessengerUtilities;
use lndk::server::setup_server;
use lndk::{setup_logger, LifecycleSignals, OfferHandler, PayOfferParams};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::time::Duration;
use tokio::{select, try_join};
use tonic_lnd::Client;

// Creates N offers and spits out the PayOfferParams that we can use to pay.
async fn create_offers(
    num: i32,
    ldk: &LdkNode,
    path_pubkeys: &Vec<PublicKey>,
    client: Client,
    reply_path_keys: &Vec<PublicKey>,
) -> Vec<PayOfferParams> {
    let mut pay_cfgs = vec![];
    for _ in 0..num {
        let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
        let offer = ldk
            .create_offer(
                path_pubkeys,
                Network::Regtest,
                20_000,
                Quantity::One,
                expiration,
            )
            .await
            .expect("should create offer");

        let messenger_utils = MessengerUtilities::new();
        let blinded_path = offer.paths()[0].clone();
        let secp_ctx = Secp256k1::new();
        let reply_path =
            BlindedPath::new_for_message(reply_path_keys, &messenger_utils, &secp_ctx).unwrap();

        let pay_cfg = PayOfferParams {
            offer: offer,
            amount: Some(20_000),
            payer_note: Some("".to_string()),
            network: Network::Regtest,
            client: client.clone(),
            destination: Destination::BlindedPath(blinded_path),
            reply_path: Some(reply_path),
            response_invoice_timeout: None,
        };

        pay_cfgs.push(pay_cfg);
    }

    return pay_cfgs;
}

// A future that pays the same offer three times concurrently.
async fn pay_same_offer(handler: Arc<OfferHandler>, pay_cfg: PayOfferParams) -> Result<(), ()> {
    let fut1 = handler.pay_offer(pay_cfg.clone());
    let fut2 = handler.pay_offer(pay_cfg.clone());
    let fut3 = handler.pay_offer(pay_cfg);

    try_join!(fut1, fut2, fut3).map(|_| ()).map_err(|_| ())
}

// A future that pays different offers concurrently.
async fn pay_offers(handler: Arc<OfferHandler>, pay_cfgs: &Vec<PayOfferParams>) -> Result<(), ()> {
    let mut futs: Vec<_> = vec![];
    for i in 0..pay_cfgs.len() {
        futs.push(handler.pay_offer(pay_cfgs[i].clone()));
    }

    try_join_all(futs).await.map(|_| ()).map_err(|_| ())
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
        skip_version_check: false,
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
    let handler = Arc::new(lndk::OfferHandler::default());
    let messenger = lndk::LndkOnionMessenger::new();
    let (invoice_request, _, _) = handler
        .create_invoice_request(
            client.clone(),
            offer.clone(),
            Network::Regtest,
            Some(20_000),
            Some("".to_string()),
        )
        .await
        .unwrap();

    let destination = Destination::BlindedPath(blinded_path.clone());
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = handler.send_invoice_request(
            destination.clone(),
            client.clone(),
            Some(reply_path.clone()),
            invoice_request,
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
        skip_version_check: false,
    };

    let log_dir = Some(
        lndk_dir
            .join(format!("lndk-logs.txt"))
            .to_str()
            .unwrap()
            .to_string(),
    );
    setup_logger(None, log_dir).unwrap();

    let handler = Arc::new(lndk::OfferHandler::default());
    let messenger = lndk::LndkOnionMessenger::new();
    let (invoice_request, _, _) = handler
        .create_invoice_request(
            client.clone(),
            offer.clone(),
            Network::Regtest,
            Some(20_000),
            Some("".to_string()),
        )
        .await
        .unwrap();
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = handler.send_invoice_request(
            destination,
            client.clone(),
            Some(reply_path.clone()),
            invoice_request,
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
        common::connect_network(&ldk1, &ldk2, true, &mut lnd, &bitcoind).await;

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
        offer: offer.clone(),
        amount: Some(20_000),
        payer_note: Some("".to_string()),
        network: Network::Regtest,
        client: client.clone(),
        destination: Destination::BlindedPath(blinded_path.clone()),
        reply_path: Some(reply_path),
        response_invoice_timeout: None,
    };
    select! {
        val = messenger.run(lndk_cfg.clone(), Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = handler.pay_offer(pay_cfg.clone()) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that we're able to pay the same offer multiple times concurrently.
async fn test_lndk_pay_offer_concurrently() {
    let test_name = "lndk_pay_offer_concurrently";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, true, &mut lnd, &bitcoind).await;

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
        offer: offer.clone(),
        amount: Some(20_000),
        payer_note: Some("".to_string()),
        network: Network::Regtest,
        client: client.clone(),
        destination: Destination::BlindedPath(blinded_path.clone()),
        reply_path: Some(reply_path),
        response_invoice_timeout: None,
    };
    // Let's also try to pay the same offer multiple times concurrently.
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = pay_same_offer(Arc::clone(&handler), pay_cfg) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that we're able to pay multiple offers at the same time.
async fn test_lndk_pay_multiple_offers_concurrently() {
    let test_name = "lndk_pay_multiple_offers_concurrently";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, true, &mut lnd, &bitcoind).await;

    let path_pubkeys = &vec![ldk2_pubkey, ldk1_pubkey];
    let reply_path = &vec![ldk2_pubkey, lnd_pubkey];
    let pay_offer_cfgs = create_offers(
        3,
        &ldk1,
        path_pubkeys,
        lnd.client.clone().unwrap(),
        reply_path,
    )
    .await;

    let (lndk_cfg, handler, messenger, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    // Let's also try to pay multiple offers at the same time.
    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res = pay_offers(Arc::clone(&handler), &pay_offer_cfgs) => {
            assert!(res.is_ok());
            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// Here we test that a new key is created with each call to create_invoice_request. Transient keys
// improve privacy and we also need them to successfully make multiple payments to the same CLN
// offer.
async fn test_transient_keys() {
    let test_name = "transient_keys";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, _) =
        common::connect_network(&ldk1, &ldk2, true, &mut lnd, &bitcoind).await;

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

    select! {
        val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
            panic!("lndk should not have completed first {:?}", val);
        },
        res1 = handler.create_invoice_request(
            lnd.client.clone().unwrap(),
            offer.clone(),
            Network::Regtest,
            None,
            None,
        ) => {
            let res2 = handler.create_invoice_request(
                lnd.client.clone().unwrap(),
                offer.clone(),
                Network::Regtest,
                None,
                None,
            ).await;

            let pubkey1 = res1.unwrap().0.payer_id();
            let pubkey2 = res2.unwrap().0.payer_id();

            // Verify that the signing pubkeys for each invoice request are different.
            assert_ne!(pubkey1, pubkey2);

            shutdown.trigger();
            ldk1.stop().await;
            ldk2.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
// We test that when creating a reply path for an offer node to send an invoice to, we don't
// use a node that we're connected to as the introduction node if it's an unadvertised node that
// is only connected by private channels.
async fn test_reply_path_unannounced_peers() {
    let test_name = "unannounced_peers";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    let (_, _, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, false, &mut lnd, &bitcoind).await;

    let (_, handler, _, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    // In the small network we produced above, the lnd node is only connected to ldk2, which has a
    // private channel and as such, is an unadvertised node. Because of that, create_reply_path
    // should not use ldk2 as an introduction node and should return a reply path directly to
    // itself.
    let reply_path = handler
        .create_reply_path(lnd.client.clone().unwrap(), lnd_pubkey)
        .await;
    assert!(reply_path.is_ok());
    assert_eq!(reply_path.unwrap().blinded_hops.len(), 1);

    shutdown.trigger();
    ldk1.stop().await;
    ldk2.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
// We test that when creating a reply path for an offer node to send an invoice to, we successfully
// use a node that we're connected to as the introduction node *if* it's an advertised node with
// public channels.
async fn test_reply_path_announced_peers() {
    let test_name = "announced_peers";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    let (_, ldk2_pubkey, lnd_pubkey) =
        common::connect_network(&ldk1, &ldk2, true, &mut lnd, &bitcoind).await;

    let (_, handler, _, shutdown) =
        common::setup_lndk(&lnd.cert_path, &lnd.macaroon_path, lnd.address, lndk_dir).await;

    // In the small network we produced above, the lnd node is only connected to ldk2, which has a
    // public channel and as such, is indeed an advertised node. Because of this, we make sure
    // create_reply_path produces a path of length two with ldk2 as the introduction node, as we
    // expected.
    let reply_path = handler
        .create_reply_path(lnd.client.clone().unwrap(), lnd_pubkey)
        .await;
    assert!(reply_path.is_ok());
    assert_eq!(reply_path.as_ref().unwrap().blinded_hops.len(), 2);
    assert_eq!(
        reply_path.as_ref().unwrap().introduction_node,
        IntroductionNode::NodeId(ldk2_pubkey)
    );

    shutdown.trigger();
    ldk1.stop().await;
    ldk2.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
// Tests the interaction between the CLI and the server and ensures that we can make a payment
// with the CLI.
async fn test_server_pay_offer() {
    let test_name = "lndk_server_pay_offer";
    let (bitcoind, mut lnd, ldk1, ldk2, lndk_dir) =
        common::setup_test_infrastructure(test_name).await;

    let (ldk1_pubkey, ldk2_pubkey, _) =
        common::connect_network(&ldk1, &ldk2, true, &mut lnd, &bitcoind).await;

    let path_pubkeys = vec![ldk2_pubkey, ldk1_pubkey];
    let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
    let offer_amount = 20_000;
    let offer = ldk1
        .create_offer(
            &path_pubkeys,
            Network::Regtest,
            offer_amount,
            Quantity::One,
            expiration,
        )
        .await
        .expect("should create offer");

    let log_dir = Some(
        lndk_dir
            .join(format!("lndk-logs.txt"))
            .to_str()
            .unwrap()
            .to_string(),
    );
    setup_logger(None, log_dir).unwrap();

    let (lndk_cfg, handler, messenger, shutdown) = common::setup_lndk(
        &lnd.cert_path,
        &lnd.macaroon_path,
        lnd.address.clone(),
        lndk_dir.clone(),
    )
    .await;

    let creds = validate_lnd_creds(
        Some(PathBuf::from_str(&lnd.cert_path).unwrap()),
        None,
        Some(PathBuf::from_str(&lnd.macaroon_path).unwrap()),
        None,
    )
    .unwrap();
    let lnd_cfg = lndk::lnd::LndCfg::new(lnd.address.clone(), creds.clone());

    let server_port = get_available_port().unwrap();
    let server_fut = setup_server(
        lnd_cfg,
        None,
        Some(server_port),
        lndk_dir.clone(),
        None,
        Arc::clone(&handler),
        lnd.address,
    )
    .await
    .unwrap();

    // Spin up the onion messenger and server in another thread before we try to pay the offer
    // via the CLI.
    tokio::spawn(async move {
        select! {
            val = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
                panic!("lndk should not have completed first {:?}", val);
            },
            _ = server_fut => {
                panic!("server should not have completed first");
            },
        }
    });

    // Let's try running the pay-offer CLI command.
    let lndk_tls_cert_path = lndk_dir.join("tls-cert.pem");
    let cli = LndkCli::new(
        lnd.macaroon_path,
        lndk_tls_cert_path.to_str().unwrap().to_owned(),
        server_port,
    );

    let resp = cli.pay_offer(offer.to_string(), offer_amount).await;
    assert!(resp.success());

    shutdown.trigger();
    ldk1.stop().await;
    ldk2.stop().await;
}
