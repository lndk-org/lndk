mod common;

#[tokio::test]
async fn test_setup() {
    // Spin up a bitcoind and lnd node, which are required for our tests.
    let (_bitcoind, _lnd, _bitcoind_dir, _electrsd, _ldk_node, _ldk_dir) =
        common::setup_test_infrastructure().await;
}
