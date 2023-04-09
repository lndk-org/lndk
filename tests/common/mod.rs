use bitcoincore_rpc::{bitcoin::Network, json, RpcApi};
use bitcoind::{BitcoinD, Conf, ConnectParams};
use flate2::read::GzDecoder;
use std::env;
use std::process::{Child, Command, Stdio};
use std::thread;
use tar::Archive;
use tempfile::{tempdir, Builder, TempDir};
use tokio::time::Duration;

const LND_VERSION: &str = "0.16.0";

// setup_test_infrastructure spins up all of the infrastructure we need to test LNDK, including a bitcoind node and two
// LND nodes. LNDK can then use this test environment to run.
pub async fn setup_test_infrastructure() -> (BitcoinD, LndNode, TempDir) {
    let (bitcoind, bitcoind_dir) = setup_bitcoind().await;
    let lnd_exe_dir = download_lnd().await;

    let lnd_node = LndNode::new(bitcoind.params.clone(), lnd_exe_dir);
    lnd_node.setup_client().await;

    return (bitcoind, lnd_node, bitcoind_dir);
}

pub async fn setup_bitcoind() -> (BitcoinD, TempDir) {
    let bitcoind_dir = tempdir().unwrap();
    let bitcoind_dir_path = bitcoind_dir.path().clone().to_path_buf();
    let mut conf = Conf::default();
    conf.tmpdir = Some(bitcoind_dir_path);
    conf.args = vec![
        "-regtest",
        "-fallbackfee=0.0001",
        "-zmqpubrawblock=tcp://127.0.0.1:28332",
        "-zmqpubrawtx=tcp://127.0.0.1:28333",
    ];
    let bitcoind = BitcoinD::with_conf(bitcoind::downloaded_exe_path().unwrap(), &conf).unwrap();

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

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn lnd_download_filename() -> String {
    format!("lnd-linux-amd64-v{}-beta.tar.gz", &LND_VERSION)
}

// download_lnd downloads the lnd binary to tmp if it doesn't exist yet. Currently it only downloads binaries
// compatible with the linux os.
pub async fn download_lnd() -> TempDir {
    let temp_dir = tempdir().unwrap();
    let lnd_dir = &temp_dir.path().to_path_buf();
    let lnd_releases_endpoint =
        "https://github.com/lightningnetwork/lnd/releases/download/v0.16.0-beta";
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
#[derive(Debug)]
pub struct LndNode {
    address: String,
    _lnd_exe_dir: TempDir,
    _lnd_dir_tmp: TempDir,
    cert_path: String,
    _handle: Child,
}

impl LndNode {
    fn new(bitcoind_connect_params: ConnectParams, lnd_exe_dir: TempDir) -> LndNode {
        env::set_current_dir(lnd_exe_dir.path().join("lnd-linux-amd64-v0.16.0-beta"))
            .expect("couldn't set current directory");

        let lnd_dir_binding = Builder::new().prefix("lnd").tempdir().unwrap();
        let lnd_dir = lnd_dir_binding.path();

        let cert_path = lnd_dir.join("tls.cert").to_str().unwrap().to_string();

        // Have node run on a randomly assigned grpc port. That way, if we run more than one lnd node, they won't
        // clash.
        let port = bitcoind::get_available_port().unwrap();
        let rpc_addr = format!("localhost:{}", port);
        let args = [
            format!("--rpclisten={}", rpc_addr),
            format!("--norest"),
            format!("--bitcoin.active"),
            format!("--bitcoin.node=bitcoind"),
            format!("--bitcoin.regtest"),
            format!("--lnddir={}", lnd_dir.display()),
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
            _handle: cmd,
        }
    }

    // Setup the client we need to interact with the LND node.
    async fn setup_client(&self) {
        // We need to give lnd some time to start up before we'll be able to interact with it via the client.
        let mut retry = false;
        let mut retry_num = 0;
        while retry_num == 0 || retry {
            thread::sleep(Duration::from_secs(2));
            let unlocker_result =
                tonic_lnd::connect_wallet_unlocker(self.address.clone(), self.cert_path.clone())
                    .await;
            match unlocker_result {
                Ok(mut unlocker) => {
                    // Generate wallet seed, which we need to initialize the lnd wallet.
                    let request = tonic_lnd::lnrpc::GenSeedRequest {
                        ..Default::default()
                    };
                    let resp = unlocker
                        .client()
                        .gen_seed(request)
                        .await
                        .expect("failed to generate seed");

                    // Attempt to initialize wallet.
                    let init_request = tonic_lnd::lnrpc::InitWalletRequest {
                        wallet_password: "password".as_bytes().to_vec(),
                        cipher_seed_mnemonic: resp.into_inner().cipher_seed_mnemonic,
                        ..Default::default()
                    };
                    let _init_resp = unlocker
                        .client()
                        .init_wallet(init_request)
                        .await
                        .expect("failed to initialize wallet");

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
