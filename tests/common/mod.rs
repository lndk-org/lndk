mod test_utils;

use bitcoin::network::constants::Network;
use bitcoin::secp256k1::PublicKey;
use bitcoincore_rpc::{bitcoin::Network as BitcoindNetwork, json, RpcApi};
use bitcoind::{get_available_port, BitcoinD, Conf, ConnectParams};
use chrono::Utc;
use ldk_sample::config::LdkUserInfo;
use ldk_sample::node_api::Node as LdkNode;
use lightning::util::logger::Level;
use lndk::lnd::validate_lnd_creds;
use lndk::{setup_logger, LifecycleSignals, LndkOnionMessenger, OfferHandler};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::{env, fs};
use tempfile::{tempdir, Builder, TempDir};
use tokio::time::{sleep, timeout, Duration};
use tonic_lnd::lnrpc::{AddressType, GetInfoRequest};
use tonic_lnd::Client;

const LNDK_TESTS_FOLDER: &str = "lndk-tests";

pub async fn setup_test_infrastructure(
    test_name: &str,
) -> (BitcoindNode, LndNode, LdkNode, LdkNode, PathBuf) {
    let bitcoind = setup_bitcoind().await;
    let (ldk_test_dir, lnd_test_dir, lndk_test_dir) = setup_test_dirs(test_name);
    let mut lnd = LndNode::new(
        bitcoind.node.params.clone(),
        bitcoind.zmq_block_port,
        bitcoind.zmq_tx_port,
        lnd_test_dir,
    );
    lnd.setup_client().await;

    let connect_params = bitcoind.node.params.get_cookie_values().unwrap();

    let port = get_available_port().unwrap();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    let ldk1_config = LdkUserInfo {
        bitcoind_rpc_username: connect_params.0.clone().unwrap(),
        bitcoind_rpc_password: connect_params.1.clone().unwrap(),
        bitcoind_rpc_host: String::from("localhost"),
        bitcoind_rpc_port: bitcoind.node.params.rpc_socket.port(),
        ldk_data_dir: ldk_test_dir.clone(),
        ldk_announced_listen_addr: vec![addr.into()],
        ldk_peer_listening_port: port,
        ldk_announced_node_name: [0; 32],
        network: Network::Regtest,
        log_level: Level::Trace,
        node_num: 1,
    };

    let port = get_available_port().unwrap();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    let ldk2_config = LdkUserInfo {
        bitcoind_rpc_username: connect_params.0.unwrap(),
        bitcoind_rpc_password: connect_params.1.unwrap(),
        bitcoind_rpc_host: String::from("localhost"),
        bitcoind_rpc_port: bitcoind.node.params.rpc_socket.port(),
        ldk_data_dir: ldk_test_dir,
        ldk_announced_listen_addr: vec![addr.into()],
        ldk_peer_listening_port: port,
        ldk_announced_node_name: [0; 32],
        network: Network::Regtest,
        log_level: Level::Trace,
        node_num: 2,
    };

    let ldk1 = ldk_sample::start_ldk(ldk1_config, test_name).await;
    let ldk2 = ldk_sample::start_ldk(ldk2_config, test_name).await;

    (bitcoind, lnd, ldk1, ldk2, lndk_test_dir)
}

// connect_network establishes connections/channels between our nodes, and mines enough blocks and
// allows the network to sync so that we can use the channels.
pub async fn connect_network(
    ldk1: &LdkNode,
    ldk2: &LdkNode,
    lnd: &mut LndNode,
    bitcoind: &BitcoindNode,
) -> (PublicKey, PublicKey, PublicKey) {
    // Here we'll produce a little network of channels:
    //
    // ldk1 <- ldk2 <- lnd
    //
    // ldk1 will be the offer creator, which will build a blinded route from ldk2 to ldk1.
    let (ldk1_pubkey, addr) = ldk1.get_node_info();
    let (ldk2_pubkey, addr_2) = ldk2.get_node_info();
    let lnd_info = lnd.get_info().await;
    let lnd_pubkey = PublicKey::from_str(&lnd_info.identity_pubkey).unwrap();

    ldk1.connect_to_peer(ldk2_pubkey, addr_2).await.unwrap();
    lnd.connect_to_peer(ldk2_pubkey, addr_2).await;

    let ldk2_fund_addr = ldk2.bitcoind_client.get_new_address().await;
    let lnd_fund_addr = lnd.new_address().await.address;

    // We need to convert funding addresses to the form that the bitcoincore_rpc library recognizes.
    let ldk2_addr_string = ldk2_fund_addr.to_string();
    let ldk2_addr = bitcoind::bitcoincore_rpc::bitcoin::Address::from_str(&ldk2_addr_string)
        .unwrap()
        .require_network(BitcoindNetwork::Regtest)
        .unwrap();
    let lnd_addr = bitcoind::bitcoincore_rpc::bitcoin::Address::from_str(&lnd_fund_addr)
        .unwrap()
        .require_network(BitcoindNetwork::Regtest)
        .unwrap();
    let lnd_network_addr = lnd
        .address
        .replace("localhost", "127.0.0.1")
        .replace("https://", "");

    // Fund both of these nodes, open the channels, and synchronize the network.
    bitcoind
        .node
        .client
        .generate_to_address(6, &lnd_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    ldk2.open_channel(ldk1_pubkey, addr, 200000, 0, false)
        .await
        .unwrap();

    lnd.wait_for_graph_sync().await;

    ldk2.open_channel(
        lnd_pubkey,
        SocketAddr::from_str(&lnd_network_addr).unwrap(),
        200000,
        10000000,
        true,
    )
    .await
    .unwrap();

    lnd.wait_for_graph_sync().await;

    bitcoind
        .node
        .client
        .generate_to_address(20, &ldk2_addr)
        .unwrap();

    lnd.wait_for_chain_sync().await;

    (ldk1_pubkey, ldk2_pubkey, lnd_pubkey)
}

pub async fn setup_lndk(
    lnd_cert_path: &str,
    lnd_macaroon_path: &str,
    address: String,
    lndk_dir: PathBuf,
) -> (
    lndk::Cfg,
    Arc<OfferHandler>,
    LndkOnionMessenger,
    triggered::Trigger,
) {
    let (shutdown, listener) = triggered::trigger();
    let creds = validate_lnd_creds(
        Some(PathBuf::from_str(lnd_cert_path).unwrap()),
        None,
        Some(PathBuf::from_str(lnd_macaroon_path).unwrap()),
        None,
    )
    .unwrap();
    let lnd_cfg = lndk::lnd::LndCfg::new(address, creds);

    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener,
    };

    let lndk_cfg = lndk::Cfg {
        lnd: lnd_cfg,
        signals,
        skip_version_check: false,
    };

    // Make sure lndk successfully sends the invoice_request.
    let handler = Arc::new(lndk::OfferHandler::default());
    let messenger = lndk::LndkOnionMessenger::new();

    let log_dir = Some(
        lndk_dir
            .join(format!("lndk-logs.txt"))
            .to_str()
            .unwrap()
            .to_string(),
    );
    setup_logger(None, log_dir).unwrap();

    return (lndk_cfg, handler, messenger, shutdown);
}

// Sets up /tmp/lndk-tests folder where we'll store the bins, data directories, and logs needed
// for our tests.
//
// The file tree structure looks like:
//
// /tmp/lndk-tests
//             |
//             +-- /bin (compiled lnd binary is stored here)
//             |
//             +-- /test-{test_name}-{time-run} (each time you run a test a new folder will be
//                   |                                           created with the data within)
//                   |
//                   +-- /lnd-data (lnd data and logs are stored here)
//                   |
//                   +-- /ldk-data (ldk data and logs are stored here)
//                   |
//                   +-- /lndk-data (lndk logs are stored here)
//
fn setup_test_dirs(test_name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let lndk_tests_dir = env::temp_dir().join(LNDK_TESTS_FOLDER);
    let bin_dir = lndk_tests_dir.join("bin");
    let now_timestamp = Utc::now();
    let timestamp = now_timestamp.format("%d-%m-%Y-%H%M");
    let itest_dir = lndk_tests_dir.join(format!("test-{test_name}-{timestamp}"));
    let ldk_data_dir = itest_dir.join("ldk-data");
    let lnd_data_dir = itest_dir.join("lnd-data");
    let lndk_data_dir = itest_dir.join("lndk-data");

    fs::create_dir_all(lndk_tests_dir.clone()).unwrap();
    fs::create_dir_all(bin_dir.clone()).unwrap();
    fs::create_dir_all(itest_dir.clone()).unwrap();
    fs::create_dir_all(ldk_data_dir.clone()).unwrap();
    fs::create_dir_all(lnd_data_dir.clone()).unwrap();
    fs::create_dir_all(lndk_data_dir.clone()).unwrap();

    (ldk_data_dir, lnd_data_dir, lndk_data_dir)
}

// BitcoindNode holds the tools we need to interact with a Bitcoind node.
pub struct BitcoindNode {
    pub node: BitcoinD,
    _data_dir: TempDir,
    zmq_block_port: u16,
    zmq_tx_port: u16,
}

pub async fn setup_bitcoind() -> BitcoindNode {
    let data_dir = tempdir().unwrap();
    let data_dir_path = data_dir.path().to_path_buf();
    let mut conf = Conf::default();
    let zmq_block_port = get_available_port().unwrap();
    let zmq_tx_port = get_available_port().unwrap();
    let zmq_block_port_arg = &format!("-zmqpubrawblock=tcp://127.0.0.1:{zmq_block_port}");
    let zmq_tx_port_arg = &format!("-zmqpubrawtx=tcp://127.0.0.1:{zmq_tx_port}");
    conf.tmpdir = Some(data_dir_path);
    conf.args = vec!["-regtest", zmq_block_port_arg, zmq_tx_port_arg];
    let bitcoind = BitcoinD::from_downloaded_with_conf(&conf).unwrap();

    // Mine 101 blocks in our little regtest network so that the funds are spendable.
    // (See https://bitcoin.stackexchange.com/questions/1991/what-is-the-block-maturation-time)
    let address = bitcoind
        .client
        .get_new_address(None, Some(json::AddressType::Bech32))
        .unwrap();
    let address = address.require_network(BitcoindNetwork::Regtest).unwrap();
    bitcoind.client.generate_to_address(101, &address).unwrap();

    BitcoindNode {
        node: bitcoind,
        _data_dir: data_dir,
        zmq_block_port,
        zmq_tx_port,
    }
}

// LndNode holds the tools we need to interact with a Lightning node.
pub struct LndNode {
    pub address: String,
    _lnd_dir_tmp: TempDir,
    pub cert_path: String,
    pub macaroon_path: String,
    _handle: Child,
    pub client: Option<Client>,
}

impl LndNode {
    fn new(
        bitcoind_connect_params: ConnectParams,
        zmq_block_port: u16,
        zmq_tx_port: u16,
        lnd_data_dir: PathBuf,
    ) -> LndNode {
        let lnd_exe_dir = env::temp_dir().join(LNDK_TESTS_FOLDER).join("bin");
        env::set_current_dir(lnd_exe_dir).expect("couldn't set current directory");

        let lnd_dir_binding = Builder::new()
            .prefix("lnd-data-")
            .tempdir_in(lnd_data_dir.clone())
            .unwrap();
        let lnd_dir = lnd_dir_binding.path();

        let macaroon_path = lnd_dir
            .join("data/chain/bitcoin/regtest/admin.macaroon")
            .to_str()
            .unwrap()
            .to_string();

        let connect_params = bitcoind_connect_params.get_cookie_values().unwrap();
        let log_dir_path_buf = lnd_data_dir.join(format!("lnd-logs"));
        let log_dir = log_dir_path_buf.as_path();
        let data_dir = lnd_dir.join("data").to_str().unwrap().to_string();
        let cert_path = lnd_dir.to_str().unwrap().to_string() + "/tls.cert";
        let key_path = lnd_dir.to_str().unwrap().to_string() + "/tls.key";

        // Have node run on a randomly assigned grpc port. That way, if we run more than one lnd
        // node, they won't clash.
        let port = bitcoind::get_available_port().unwrap();
        let rpc_addr = format!("localhost:{}", port);
        let lnd_port = bitcoind::get_available_port().unwrap();
        let lnd_addr = format!("localhost:{}", lnd_port);
        let args = [
            format!("--listen={}", lnd_addr),
            format!("--rpclisten={}", rpc_addr),
            format!("--norest"),
            // With this flag, we don't have to unlock the wallet on startup.
            format!("--noseedbackup"),
            format!("--bitcoin.active"),
            format!("--bitcoin.node=bitcoind"),
            format!("--bitcoin.regtest"),
            format!("--datadir={}", data_dir),
            format!("--tlscertpath={}", cert_path),
            format!("--tlskeypath={}", key_path),
            format!("--logdir={}", log_dir.display()),
            format!("--debuglevel=info,PEER=info"),
            format!("--bitcoind.rpcuser={}", connect_params.0.unwrap()),
            format!("--bitcoind.rpcpass={}", connect_params.1.unwrap()),
            format!(
                "--bitcoind.zmqpubrawblock=tcp://127.0.0.1:{}",
                zmq_block_port
            ),
            format!("--bitcoind.zmqpubrawtx=tcp://127.0.0.1:{}", zmq_tx_port),
            format!(
                "--bitcoind.rpchost={:?}",
                bitcoind_connect_params.rpc_socket
            ),
            format!("--protocol.custom-message=513"),
            format!("--protocol.custom-nodeann=39"),
            format!("--protocol.custom-init=39"),
        ];

        // TODO: For Windows we might need to add ".exe" at the end.
        let cmd = Command::new("./lnd-itest")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to execute lnd command");

        LndNode {
            address: format!("https://{}", rpc_addr),
            _lnd_dir_tmp: lnd_dir_binding,
            cert_path: cert_path,
            macaroon_path: macaroon_path,
            _handle: cmd,
            client: None,
        }
    }

    // Setup the client we need to interact with the LND node.
    async fn setup_client(&mut self) {
        // We need to give lnd some time to start up before we'll be able to interact with it via
        // the client.
        let mut retry = false;
        let mut retry_num = 0;
        while retry_num == 0 || retry {
            thread::sleep(Duration::from_secs(3));

            let client_result = tonic_lnd::connect(
                self.address.clone(),
                self.cert_path.clone(),
                self.macaroon_path.clone(),
            )
            .await;

            match client_result {
                Ok(client) => {
                    self.client = Some(client);

                    retry = false;
                    retry_num += 1;
                }
                Err(err) => {
                    println!(
                        "getting client error {err}, retrying call {} time",
                        retry_num
                    );
                    if retry_num == 6 {
                        panic!("could not set up client: {err}")
                    }
                    retry = true;
                    retry_num += 1;
                }
            }
        }
    }

    #[allow(dead_code)]
    pub async fn get_info(&mut self) -> tonic_lnd::lnrpc::GetInfoResponse {
        let resp = if let Some(client) = self.client.clone() {
            let get_info_req = GetInfoRequest {};
            let make_request = || async {
                client
                    .clone()
                    .lightning()
                    .get_info(get_info_req.clone())
                    .await
            };
            let resp = test_utils::retry_async(make_request, String::from("get_info"));
            resp.await.unwrap()
        } else {
            panic!("No client")
        };

        resp
    }

    // connect_to_peer connects to the specified peer.
    pub async fn connect_to_peer(
        &mut self,
        node_id: PublicKey,
        addr: SocketAddr,
    ) -> tonic_lnd::lnrpc::ConnectPeerResponse {
        let ln_addr = tonic_lnd::lnrpc::LightningAddress {
            pubkey: node_id.to_string(),
            host: addr.to_string(),
        };

        let connect_req = tonic_lnd::lnrpc::ConnectPeerRequest {
            addr: Some(ln_addr),
            timeout: 20,
            ..Default::default()
        };

        let resp = if let Some(client) = self.client.clone() {
            let make_request = || async {
                client
                    .clone()
                    .lightning()
                    .connect_peer(connect_req.clone())
                    .await
            };
            let resp = test_utils::retry_async(make_request, String::from("connect_peer"));
            resp.await.unwrap()
        } else {
            panic!("No client")
        };

        resp
    }

    // disconnect_peer disconnects the specified peer.
    #[allow(dead_code)]
    pub async fn disconnect_peer(
        &mut self,
        node_id: PublicKey,
    ) -> tonic_lnd::lnrpc::DisconnectPeerResponse {
        let disconnect_req = tonic_lnd::lnrpc::DisconnectPeerRequest {
            pub_key: node_id.to_string(),
            ..Default::default()
        };

        let resp = if let Some(client) = self.client.clone() {
            let make_request = || async {
                client
                    .clone()
                    .lightning()
                    .disconnect_peer(disconnect_req.clone())
                    .await
            };
            let resp = test_utils::retry_async(make_request, String::from("disconnect_peer"));
            resp.await.unwrap()
        } else {
            panic!("No client")
        };

        resp
    }

    // wait_for_chain_sync waits until we're synced to chain according to the get_info response.
    // We'll timeout if it takes too long.
    pub async fn wait_for_chain_sync(&mut self) {
        match timeout(Duration::from_secs(100), self.check_chain_sync()).await {
            Err(_) => panic!("timeout before lnd synced to chain"),
            _ => {}
        };
    }

    pub async fn check_chain_sync(&mut self) {
        loop {
            let resp = self.get_info().await;
            if resp.synced_to_chain {
                return;
            }
            sleep(Duration::from_secs(2)).await;
        }
    }

    // wait_for_lnd_sync waits until we're synced to graph according to the get_info response.
    // We'll timeout if it takes too long.
    pub async fn wait_for_graph_sync(&mut self) {
        match timeout(Duration::from_secs(100), self.check_graph_sync()).await {
            Err(_) => panic!("timeout before lnd synced to graph"),
            _ => {}
        };
    }

    pub async fn check_graph_sync(&mut self) {
        loop {
            let resp = self.get_info().await;
            if resp.synced_to_graph {
                return;
            }
            sleep(Duration::from_secs(2)).await;
        }
    }

    // Create an on-chain bitcoin address to fund our LND node.
    #[allow(dead_code)]
    pub async fn new_address(&mut self) -> tonic_lnd::lnrpc::NewAddressResponse {
        let addr_req = tonic_lnd::lnrpc::NewAddressRequest {
            r#type: AddressType::TaprootPubkey.into(),
            ..Default::default()
        };

        let resp = if let Some(client) = self.client.clone() {
            let make_request = || async {
                client
                    .clone()
                    .lightning()
                    .new_address(addr_req.clone())
                    .await
            };
            let resp = test_utils::retry_async(make_request, String::from("new_address"));
            resp.await.unwrap()
        } else {
            panic!("No client")
        };

        resp
    }
}
