use bitcoincore_rpc::{bitcoin::Network, json, RpcApi};
use bitcoind::{get_available_port, BitcoinD, Conf};
use tempfile::{tempdir, TempDir};

pub async fn setup_test_infrastructure() -> (BitcoinD, TempDir) {
    setup_bitcoind().await
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
    let address = address.require_network(Network::Regtest).unwrap();
    bitcoind.client.generate_to_address(101, &address).unwrap();

    (bitcoind, bitcoind_dir)
}
