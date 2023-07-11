use bitcoincore_rpc::{bitcoin::Network, json, RpcApi};
use bitcoind::{BitcoinD, Conf, ConnectParams};
use chrono::Utc;
use electrsd::ElectrsD;
use flate2::read::GzDecoder;
use ldk_node::bitcoin::Network as LdkNetwork;
use ldk_node::io::SqliteStore;
use ldk_node::{Builder as LdkBuilder, NetAddress, Node};
use std::env;
use std::fs::File;
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::thread;
use tar::Archive;
use tempfile::{tempdir, Builder, TempDir};
use tokio::time::Duration;
use tonic_lnd::Client;

const LND_VERSION: &str = "0.16.2";

// setup_test_infrastructure spins up all of the infrastructure we need to test LNDK, including a bitcoind node and two
// LND nodes. LNDK can then use this test environment to run.
//
// Notes for developers looking for associated logs:
// - Logs for LND and LDK for the integration tests live in /tmp (or whatever the temporary directory is in the
// corresponding OS).
// - The "test_name" parameter is required to distinguish the logs coming from different integration tests.
pub async fn setup_test_infrastructure(
    test_name: String,
) -> (
    BitcoinD,
    LndNode,
    TempDir,
    ElectrsD,
    Node<SqliteStore>,
    TempDir,
) {
    let (bitcoind, bitcoind_dir) = setup_bitcoind().await;
    let lnd_exe_dir = download_lnd().await;

    let mut lnd_node = LndNode::new(bitcoind.params.clone(), lnd_exe_dir, test_name.clone());
    lnd_node.setup_client().await;

    // We also need to set up electrs, because that's the way ldk-node (currently) communicates with bitcoind to get
    // bitcoin blocks and transactions.
    let electrsd = setup_electrs().await;
    let esplora_url = format!("http://{}", electrsd.esplora_url.as_ref().unwrap());
    let (ldk_node, ldk_dir) = setup_ldk(esplora_url, test_name).await;

    return (
        bitcoind,
        lnd_node,
        bitcoind_dir,
        electrsd,
        ldk_node,
        ldk_dir,
    );
}

pub async fn setup_bitcoind() -> (BitcoinD, TempDir) {
    let bitcoind_dir = tempdir().unwrap();
    let bitcoind_dir_path = bitcoind_dir.path().clone().to_path_buf();
    let mut conf = Conf::default();
    conf.tmpdir = Some(bitcoind_dir_path);
    conf.args = vec![
        "-regtest",
        "-zmqpubrawblock=tcp://127.0.0.1:28332",
        "-zmqpubrawtx=tcp://127.0.0.1:28333",
    ];
    let bitcoind = BitcoinD::from_downloaded_with_conf(&conf).unwrap();

    // Mine 101 blocks in our little regtest network so that the funds are spendable.
    // (See https://bitcoin.stackexchange.com/questions/1991/what-is-the-block-maturation-time)
    let address = bitcoind
        .client
        .get_new_address(None, Some(json::AddressType::Bech32))
        .unwrap();
    let address = address.require_network(Network::Regtest).unwrap();
    bitcoind.client.generate_to_address(101, &address).unwrap();

    (bitcoind, bitcoind_dir)
}

// setup_electrs sets up the electrs instance required (for the time being) for us to connect to an LDK node.
pub async fn setup_electrs() -> ElectrsD {
    let bitcoind_exe = env::var("BITCOIND_EXE")
        .ok()
        .or_else(|| electrsd::bitcoind::downloaded_exe_path().ok())
        .expect(
            "you need to provide an env var BITCOIND_EXE or specify a bitcoind version feature",
        );
    let bitcoind_dir = tempdir().unwrap();
    let bitcoind_dir_path = bitcoind_dir.path().clone().to_path_buf();
    let mut bitcoind_conf = electrsd::bitcoind::Conf::default();
    let port = bitcoind::get_available_port().unwrap();
    let zmq_block_port = bitcoind::get_available_port().unwrap();
    let zmq_tx_port = bitcoind::get_available_port().unwrap();
    let port_str = format!("-port={}", port);
    let bind_str = format!("-bind=127.0.0.1:{}=onion", port);
    let zmq_block_str = format!("-zmqpubrawblock=tcp://127.0.0.1:{}", zmq_block_port);
    let zmq_tx_str = format!("-zmqpubrawtx=tcp://127.0.0.1:{}", zmq_tx_port);
    bitcoind_conf.tmpdir = Some(bitcoind_dir_path);
    bitcoind_conf.p2p = electrsd::bitcoind::P2P::Yes;
    bitcoind_conf.args = vec![
        "-server",
        "-regtest",
        &port_str,
        &bind_str,
        &zmq_block_str,
        &zmq_tx_str,
    ];
    let bitcoind = electrsd::bitcoind::BitcoinD::with_conf(bitcoind_exe, &bitcoind_conf).unwrap();

    let electrs_exe =
        electrsd::downloaded_exe_path().expect("electrs version feature must be enabled");
    let mut electrsd_conf = electrsd::Conf::default();
    let electrsd_dir = tempdir().unwrap();
    let electrsd_dir_path = electrsd_dir.path().clone().to_path_buf();
    electrsd_conf.tmpdir = Some(electrsd_dir_path);
    electrsd_conf.http_enabled = true;
    electrsd_conf.network = "regtest";

    ElectrsD::with_conf(electrs_exe, &bitcoind, &electrsd_conf).unwrap()
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn lnd_download_filename() -> String {
    format!("lnd-linux-amd64-v{}-beta.tar.gz", &LND_VERSION)
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn lnd_download_filename() -> String {
    format!("lnd-darwin-arm64-v{}-beta.tar.gz", &LND_VERSION)
}

#[cfg(all(target_os = "windows"))]
fn lnd_download_filename() -> String {
    panic!("Running integration tests on Windows os is not currently supported");
}

// download_lnd downloads the lnd binary to tmp if it doesn't exist yet. Currently it only downloads binaries
// compatible with the linux os.
pub async fn download_lnd() -> TempDir {
    let temp_dir = tempdir().unwrap();
    let lnd_dir = &temp_dir.path().to_path_buf();
    let lnd_releases_endpoint =
        "https://github.com/lightningnetwork/lnd/releases/download/v0.16.2-beta";
    let lnd_download_endpoint = format!("{lnd_releases_endpoint}/{}", lnd_download_filename());

    let resp = minreq::get(&lnd_download_endpoint).send().unwrap();
    assert_eq!(
        resp.status_code, 200,
        "url {lnd_download_endpoint} didn't return 200"
    );
    let content = resp.as_bytes();

    let decoder = GzDecoder::new(&content[..]);
    let mut archive = Archive::new(decoder);
    archive.unpack(lnd_dir).expect("unable to unpack lnd files");

    temp_dir
}

// LndNode holds the tools we need to interact with a Lightning node.
pub struct LndNode {
    address: String,
    _lnd_exe_dir: TempDir,
    _lnd_dir_tmp: TempDir,
    cert_path: String,
    macaroon_path: String,
    _handle: Child,
    client: Option<Client>,
}

impl LndNode {
    fn new(
        bitcoind_connect_params: ConnectParams,
        lnd_exe_dir: TempDir,
        test_name: String,
    ) -> LndNode {
        env::set_current_dir(lnd_exe_dir.path().join("lnd-linux-amd64-v0.16.2-beta"))
            .expect("couldn't set current directory");

        let lnd_dir_binding = Builder::new().prefix("lnd").tempdir().unwrap();
        let lnd_dir = lnd_dir_binding.path();

        let now_timestamp = Utc::now();
        let timestamp = now_timestamp.format("%d-%m-%Y-%H-%M");
        let lnd_log_dir = env::temp_dir().join(format!("lnd_logs"));
        let log_dir_path_buf = lnd_log_dir.join(format!("lnd-logs-{test_name}-{timestamp}"));
        let log_dir = log_dir_path_buf.as_path();
        let data_dir = lnd_dir.join("data").to_str().unwrap().to_string();
        let cert_path = lnd_dir.to_str().unwrap().to_string() + "/tls.cert";
        let key_path = lnd_dir.to_str().unwrap().to_string() + "/tls.key";
        let config_path = lnd_dir.to_str().unwrap().to_string() + "/lnd.conf";
        let macaroon_path = lnd_dir
            .join("data/chain/bitcoin/regtest/admin.macaroon")
            .to_str()
            .unwrap()
            .to_string();
        let _file = File::create(config_path.clone()).unwrap();

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
            format!("--configfile={}", config_path),
            format!("--logdir={}", log_dir.display()),
            format!(
                "--bitcoind.rpccookie={}",
                bitcoind_connect_params.cookie_file.display()
            ),
            format!("--bitcoind.zmqpubrawblock=tcp://127.0.0.1:28332"),
            format!("--bitcoind.zmqpubrawtx=tcp://127.0.0.1:28333"),
            format!(
                "--bitcoind.rpchost={:?}",
                bitcoind_connect_params.rpc_socket
            ),
        ];

        let cmd = Command::new("./lnd")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to execute lnd command");

        LndNode {
            address: format!("https://{}", rpc_addr),
            _lnd_exe_dir: lnd_exe_dir,
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
}

// setup_ldk sets up an ldk node.
async fn setup_ldk(esplora_url: String, test_name: String) -> (Node<SqliteStore>, TempDir) {
    let mut builder = LdkBuilder::new();
    builder.set_network(LdkNetwork::Regtest);
    builder.set_esplora_server(esplora_url);

    let ldk_dir = tempdir().unwrap();
    let ldk_dir_path = ldk_dir.path().to_str().unwrap().to_string();

    let now_timestamp = Utc::now();
    let timestamp = now_timestamp.format("%d-%m-%Y-%H-%M");
    let ldk_log_dir = env::temp_dir().join(format!("ldk_logs"));
    let ldk_log_dir_path = ldk_log_dir
        .join(format!("ldk-logs-{test_name}-{timestamp}"))
        .into_os_string()
        .into_string()
        .unwrap();

    builder.set_storage_dir_path(ldk_dir_path);
    builder.set_log_dir_path(ldk_log_dir_path);

    let open_port = bitcoind::get_available_port().unwrap();
    let listening_addr = NetAddress::from_str(&format!("127.0.0.1:{open_port}")).unwrap();
    builder.set_listening_address(listening_addr);

    let node = builder.build();
    (node.unwrap(), ldk_dir)
}
