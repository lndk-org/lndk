mod test_utils;

use bitcoin::network::constants::Network;
use bitcoin::secp256k1::PublicKey;
use bitcoincore_rpc::{bitcoin::Network as BitcoindNetwork, json, RpcApi};
use bitcoind::{get_available_port, BitcoinD, Conf, ConnectParams};
use chrono::Utc;
use ldk_sample::config::LdkUserInfo;
use ldk_sample::node_api::Node as LdkNode;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::{env, fs};
use tempfile::{tempdir, Builder, TempDir};
use tokio::time::Duration;
use tonic_lnd::lnrpc::GetInfoRequest;
use tonic_lnd::Client;

const LNDK_TESTS_FOLDER: &str = "lndk-tests";

pub async fn setup_test_infrastructure(
    test_name: &str,
) -> (BitcoindNode, LndNode, LdkNode, LdkNode, PathBuf) {
    let bitcoind = setup_bitcoind().await;
    let (ldk_test_dir, lnd_test_dir, lndk_test_dir) = setup_test_dirs();
    let mut lnd = LndNode::new(
        bitcoind.node.params.clone(),
        bitcoind.zmq_block_port,
        bitcoind.zmq_tx_port,
        lnd_test_dir,
        test_name.clone(),
    );
    lnd.setup_client().await;

    let connect_params = bitcoind.node.params.get_cookie_values().unwrap();

    let ldk1_config = LdkUserInfo {
        bitcoind_rpc_username: connect_params.0.clone().unwrap(),
        bitcoind_rpc_password: connect_params.1.clone().unwrap(),
        bitcoind_rpc_host: String::from("localhost"),
        bitcoind_rpc_port: bitcoind.node.params.rpc_socket.port(),
        ldk_data_dir: ldk_test_dir.clone(),
        ldk_announced_listen_addr: Vec::new(),
        ldk_peer_listening_port: get_available_port().unwrap(),
        ldk_announced_node_name: [0; 32],
        network: Network::Regtest,
    };

    let ldk2_config = LdkUserInfo {
        bitcoind_rpc_username: connect_params.0.unwrap(),
        bitcoind_rpc_password: connect_params.1.unwrap(),
        bitcoind_rpc_host: String::from("localhost"),
        bitcoind_rpc_port: bitcoind.node.params.rpc_socket.port(),
        ldk_data_dir: ldk_test_dir,
        ldk_announced_listen_addr: Vec::new(),
        ldk_peer_listening_port: get_available_port().unwrap(),
        ldk_announced_node_name: [0; 32],
        network: Network::Regtest,
    };

    let ldk1 = ldk_sample::start_ldk(ldk1_config, test_name).await;
    let ldk2 = ldk_sample::start_ldk(ldk2_config, test_name).await;

    (bitcoind, lnd, ldk1, ldk2, lndk_test_dir)
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
//             +-- /lnd-data (lnd data and logs are stored here)
//             |
//             +-- /ldk-data (ldk data and logs are stored here)
//             |
//             +-- /lndk-data (lndk logs are stored here)
//
fn setup_test_dirs() -> (PathBuf, PathBuf, PathBuf) {
    let lndk_tests_dir = env::temp_dir().join(LNDK_TESTS_FOLDER);
    let bin_dir = lndk_tests_dir.join("bin");
    let ldk_data_dir = lndk_tests_dir.join("ldk-data");
    let lnd_data_dir = lndk_tests_dir.join("lnd-data");
    let lndk_data_dir = lndk_tests_dir.join("lndk-data");

    fs::create_dir_all(lndk_tests_dir.clone()).unwrap();
    fs::create_dir_all(bin_dir.clone()).unwrap();
    fs::create_dir_all(ldk_data_dir.clone()).unwrap();
    fs::create_dir_all(lnd_data_dir.clone()).unwrap();
    fs::create_dir_all(lndk_data_dir.clone()).unwrap();

    (ldk_data_dir, lnd_data_dir, lndk_data_dir)
}

// BitcoindNode holds the tools we need to interact with a Bitcoind node.
pub struct BitcoindNode {
    node: BitcoinD,
    _data_dir: TempDir,
    zmq_block_port: u16,
    zmq_tx_port: u16,
}

pub async fn setup_bitcoind() -> BitcoindNode {
    let data_dir = tempdir().unwrap();
    let data_dir_path = data_dir.path().clone().to_path_buf();
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
    client: Option<Client>,
}

impl LndNode {
    fn new(
        bitcoind_connect_params: ConnectParams,
        zmq_block_port: u16,
        zmq_tx_port: u16,
        lnd_data_dir: PathBuf,
        test_name: &str,
    ) -> LndNode {
        let lnd_exe_dir = env::temp_dir().join("lndk-tests").join("bin");
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
        let now_timestamp = Utc::now();
        let timestamp = now_timestamp.format("%d-%m-%Y-%H%M");
        let log_dir_path_buf = lnd_data_dir.join(format!("lnd-logs-{test_name}-{timestamp}"));
        let log_dir = log_dir_path_buf.as_path();
        let data_dir = lnd_dir.join("data").to_str().unwrap().to_string();
        let cert_path = lnd_dir.to_str().unwrap().to_string() + "/tls.cert";
        let key_path = lnd_dir.to_str().unwrap().to_string() + "/tls.key";

        // Have node run on a randomly assigned grpc port. That way, if we run more than one lnd node, they won't
        // clash.
        let port = bitcoind::get_available_port().unwrap();
        let rpc_addr = format!("localhost:{}", port);
        let args = [
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
        // We need to give lnd some time to start up before we'll be able to interact with it via the client.
        let mut retry = false;
        let mut retry_num = 0;
        while retry_num == 0 || retry {
            thread::sleep(Duration::from_secs(2));

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
                    if retry_num == 3 {
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
}
