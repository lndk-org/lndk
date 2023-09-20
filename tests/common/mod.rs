use bitcoin::network::constants::Network;
use bitcoincore_rpc::{bitcoin::Network as BitcoindNetwork, json, RpcApi};
use bitcoind::{get_available_port, BitcoinD, Conf};
use ldk_sample::config::LdkUserInfo;
use ldk_sample::node_api::Node as LdkNode;
use tempfile::{tempdir, TempDir};

pub async fn setup_test_infrastructure(test_name: &str) -> (BitcoinD, TempDir, LdkNode, LdkNode) {
    let (bitcoind, bitcoind_dir) = setup_bitcoind().await;
    let connect_params = bitcoind.params.get_cookie_values().unwrap();
    let ldk1_config = LdkUserInfo {
        bitcoind_rpc_username: connect_params.0.clone().unwrap(),
        bitcoind_rpc_password: connect_params.1.clone().unwrap(),
        bitcoind_rpc_host: String::from("localhost"),
        bitcoind_rpc_port: bitcoind.params.rpc_socket.port(),
        ldk_announced_listen_addr: Vec::new(),
        ldk_peer_listening_port: get_available_port().unwrap(),
        ldk_announced_node_name: [0; 32],
        network: Network::Regtest,
    };

    let ldk2_config = LdkUserInfo {
        bitcoind_rpc_username: connect_params.0.unwrap(),
        bitcoind_rpc_password: connect_params.1.unwrap(),
        bitcoind_rpc_host: String::from("localhost"),
        bitcoind_rpc_port: bitcoind.params.rpc_socket.port(),
        ldk_announced_listen_addr: Vec::new(),
        ldk_peer_listening_port: get_available_port().unwrap(),
        ldk_announced_node_name: [0; 32],
        network: Network::Regtest,
    };

    let ldk1 = ldk_sample::start_ldk(ldk1_config, test_name).await;
    let ldk2 = ldk_sample::start_ldk(ldk2_config, test_name).await;

    (bitcoind, bitcoind_dir, ldk1, ldk2)
}

pub async fn setup_bitcoind() -> (BitcoinD, TempDir) {
    let bitcoind_dir = tempdir().unwrap();
    let bitcoind_dir_path = bitcoind_dir.path().clone().to_path_buf();
    let mut conf = Conf::default();
    let zmq_block_port = get_available_port().unwrap();
    let zmq_tx_port = get_available_port().unwrap();
    let zmq_block_port_arg = &format!("-zmqpubrawblock=tcp://127.0.0.1:{zmq_block_port}");
    let zmq_tx_port_arg = &format!("-zmqpubrawtx=tcp://127.0.0.1:{zmq_tx_port}");
    conf.tmpdir = Some(bitcoind_dir_path);
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

    (bitcoind, bitcoind_dir)
}
