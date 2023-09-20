mod common;

#[tokio::test(flavor = "multi_thread")]
async fn test_ldk_send_onion_message() {
    let test_name = "send_onion_message";
    let (_bitcoind, _lnd, ldk1, ldk2) = common::setup_test_infrastructure(test_name).await;
    let (node_id_2, node_addr_2) = ldk2.get_node_info();
    ldk1.connect_to_peer(node_id_2, node_addr_2).await.unwrap();

    let data: Vec<u8> = vec![72, 101, 108, 108, 111];
    let res = ldk1.send_onion_message(vec![node_id_2], 65, data).await;
    assert!(res.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ldk_lnd_connect() {
    let test_name = "ldk_lnd_connect";
    let (_bitcoind, mut lnd, ldk1, _ldk2) = common::setup_test_infrastructure(test_name).await;
    let (pubkey, addr) = ldk1.get_node_info();
    lnd.connect_to_peer(pubkey, addr).await;
}
