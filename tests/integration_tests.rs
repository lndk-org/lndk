mod common;

#[tokio::test]
async fn test_ldk_onion_messages() {
    let (_bitcoind, _bitcoin_dir) = common::setup_test_infrastructure().await;
}
