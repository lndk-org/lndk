use bitcoin::secp256k1::{PublicKey, Secp256k1};
use futures::executor::block_on;
use lightning::blinded_path::message::{BlindedMessagePath, MessageContext};
use lightning::blinded_path::IntroductionNode;
use lightning::ln::msgs::SocketAddress;
use lightning::onion_message::messenger::{
    Destination, MessageRouter as LightningMessageRouter, OnionMessagePath,
};
use std::cell::RefCell;
use std::str::FromStr;

use crate::lnd::{features_support_onion_messages, PeerConnector};

pub struct MessageRouter<MR: LightningMessageRouter, C: PeerConnector> {
    inner_message_router: MR,
    client: RefCell<C>,
}

impl<MR: LightningMessageRouter, C: PeerConnector> MessageRouter<MR, C> {
    pub fn new(inner_message_router: MR, client: C) -> Self {
        Self {
            inner_message_router,
            // We use RefCell to allow the client to be borrowed mutably because
            // find_path trait requires that self is not mutable.
            client: RefCell::new(client),
        }
    }

    fn get_first_node(&self, destination: &Destination) -> Option<PublicKey> {
        match destination {
            Destination::Node(node_id) => Some(*node_id),
            Destination::BlindedPath(path) => match path.introduction_node() {
                IntroductionNode::NodeId(pubkey) => Some(*pubkey),
                IntroductionNode::DirectedShortChannelId(..) => None,
            },
        }
    }
}

impl<MR: LightningMessageRouter, C: PeerConnector> LightningMessageRouter for MessageRouter<MR, C> {
    fn find_path(
        &self,
        sender: PublicKey,
        peers: Vec<PublicKey>,
        destination: Destination,
    ) -> Result<OnionMessagePath, ()> {
        let first_node = match self.get_first_node(&destination) {
            Some(first_node) => first_node,
            None => return Err(()),
        };

        if peers.contains(&first_node) || sender == first_node {
            Ok(OnionMessagePath {
                intermediate_nodes: vec![],
                destination,
                first_node_addresses: None,
            })
        } else {
            let node_info = block_on(
                self.client
                    .borrow_mut()
                    .get_node_info(first_node.to_string(), false),
            )
            .map_err(|_| ())?;

            match node_info.node {
                Some(node)
                    if !node.addresses.is_empty()
                        && features_support_onion_messages(&node.features) =>
                {
                    let addresses: Vec<SocketAddress> = node
                        .addresses
                        .iter()
                        .filter_map(|addr| SocketAddress::from_str(&addr.addr).ok())
                        .collect();

                    if addresses.is_empty() {
                        return Err(());
                    }

                    Ok(OnionMessagePath {
                        intermediate_nodes: vec![],
                        destination,
                        first_node_addresses: Some(addresses),
                    })
                }
                _ => Err(()),
            }
        }
    }

    fn create_blinded_paths<T: bitcoin::secp256k1::Signing + bitcoin::secp256k1::Verification>(
        &self,
        recipient: PublicKey,
        context: MessageContext,
        peers: Vec<PublicKey>,
        secp_ctx: &Secp256k1<T>,
    ) -> Result<Vec<BlindedMessagePath>, ()> {
        self.inner_message_router
            .create_blinded_paths(recipient, context, peers, secp_ctx)
    }

    fn create_compact_blinded_paths<
        T: bitcoin::secp256k1::Signing + bitcoin::secp256k1::Verification,
    >(
        &self,
        recipient: PublicKey,
        context: MessageContext,
        peers: Vec<lightning::blinded_path::message::MessageForwardNode>,
        secp_ctx: &Secp256k1<T>,
    ) -> Result<Vec<BlindedMessagePath>, ()> {
        let peers = peers
            .into_iter()
            .map(
                |lightning::blinded_path::message::MessageForwardNode {
                     node_id,
                     short_channel_id: _,
                 }| node_id,
            )
            .collect();
        self.create_blinded_paths(recipient, context, peers, secp_ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use mockall::mock;
    use std::sync::{Arc, Mutex};

    // We manually implement the LigthtningMessageRouter mock as it
    // was simpler to implement than to use the mockall crate.
    struct MockMessageRouter {
        create_blinded_paths_calls: Arc<Mutex<u32>>,
        create_blinded_paths_result: Result<Vec<BlindedMessagePath>, ()>,
    }

    impl MockMessageRouter {
        fn new(result: Result<Vec<BlindedMessagePath>, ()>) -> Self {
            Self {
                create_blinded_paths_calls: Arc::new(Mutex::new(0)),
                create_blinded_paths_result: result,
            }
        }

        fn create_blinded_paths_call_count(&self) -> u32 {
            *self.create_blinded_paths_calls.lock().unwrap()
        }
    }

    impl LightningMessageRouter for MockMessageRouter {
        fn find_path(
            &self,
            _sender: PublicKey,
            _peers: Vec<PublicKey>,
            _destination: Destination,
        ) -> Result<OnionMessagePath, ()> {
            Err(())
        }

        fn create_blinded_paths<
            T: bitcoin::secp256k1::Signing + bitcoin::secp256k1::Verification,
        >(
            &self,
            _recipient: PublicKey,
            _context: MessageContext,
            _peers: Vec<PublicKey>,
            _secp_ctx: &Secp256k1<T>,
        ) -> Result<Vec<BlindedMessagePath>, ()> {
            *self.create_blinded_paths_calls.lock().unwrap() += 1;
            self.create_blinded_paths_result.clone()
        }
    }

    mock! {
        pub TestPeerConnector {}

        #[async_trait::async_trait]
        impl PeerConnector for TestPeerConnector {
            async fn list_peers(&mut self) -> Result<tonic_lnd::lnrpc::ListPeersResponse, tonic_lnd::tonic::Status>;
            async fn connect_peer(&mut self, node_id: String, addr: String) -> Result<(), tonic_lnd::tonic::Status>;
            async fn get_node_info(&mut self, pub_key: String, include_channels: bool) -> Result<tonic_lnd::lnrpc::NodeInfo, tonic_lnd::tonic::Status>;
        }
    }

    fn create_secp_ctx() -> Secp256k1<bitcoin::secp256k1::All> {
        Secp256k1::new()
    }

    #[test]
    fn test_create_blinded_paths_calls_inner_router() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mock_client = MockTestPeerConnector::new();

        let secp_ctx = Secp256k1::new();

        // Create test recipient
        let recipient_secret = SecretKey::from_slice(&[1; 32]).unwrap();
        let recipient = PublicKey::from_secret_key(&secp_ctx, &recipient_secret);

        // Create test peer
        let peer_secret = SecretKey::from_slice(&[2; 32]).unwrap();
        let peer_node_id = PublicKey::from_secret_key(&secp_ctx, &peer_secret);
        let peers = vec![peer_node_id];

        let context = MessageContext::Custom(vec![]);

        let message_router = MessageRouter::new(mock_router, mock_client);

        // Call create_blinded_paths
        let result = message_router.create_blinded_paths(recipient, context, peers, &secp_ctx);

        // Verify that the inner router was called exactly once
        assert_eq!(
            message_router
                .inner_message_router
                .create_blinded_paths_call_count(),
            1
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_blinded_paths_forwards_error() {
        let mock_router = MockMessageRouter::new(Err(()));
        let mock_client = MockTestPeerConnector::new();

        let secp_ctx = Secp256k1::new();

        // Create test recipient
        let recipient_secret = SecretKey::from_slice(&[1; 32]).unwrap();
        let recipient = PublicKey::from_secret_key(&secp_ctx, &recipient_secret);

        let peers = vec![];
        let context = MessageContext::Custom(vec![]);

        let message_router = MessageRouter::new(mock_router, mock_client);

        // Call create_blinded_paths
        let result = message_router.create_blinded_paths(recipient, context, peers, &secp_ctx);

        // Verify that the inner router was called and the error was forwarded
        assert_eq!(
            message_router
                .inner_message_router
                .create_blinded_paths_call_count(),
            1
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_get_first_node_with_node_destination() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mock_client = MockTestPeerConnector::new();
        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();
        let node_secret = SecretKey::from_slice(&[1; 32]).unwrap();
        let node_pubkey = PublicKey::from_secret_key(&secp_ctx, &node_secret);
        let destination = Destination::Node(node_pubkey);

        let first_node = message_router.get_first_node(&destination);
        assert_eq!(first_node, Some(node_pubkey));
    }

    // Test the DirectedShortChannelId case by testing get_first_node function directly
    // This is simpler than creating a full BlindedMessagePath with DirectedShortChannelId
    // For now we test other cases that are easier to implement

    #[test]
    fn test_find_path_sender_equals_first_node() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mock_client = MockTestPeerConnector::new();
        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        // Use the same key as sender for destination
        let destination = Destination::Node(sender);

        let peers = vec![];

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_ok());

        let path = result.unwrap();
        assert!(path.intermediate_nodes.is_empty());
        assert!(path.first_node_addresses.is_none());
    }

    #[test]
    fn test_find_path_peers_contains_first_node() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mock_client = MockTestPeerConnector::new();
        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        let destination_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let destination_node = PublicKey::from_secret_key(&secp_ctx, &destination_secret);
        let destination = Destination::Node(destination_node);

        // Include destination node in peers
        let peers = vec![destination_node];

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_ok());

        let path = result.unwrap();
        assert!(path.intermediate_nodes.is_empty());
        assert!(path.first_node_addresses.is_none());
    }

    #[test]
    fn test_find_path_get_node_info_returns_error() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mut mock_client = MockTestPeerConnector::new();

        // Mock get_node_info to return an error
        mock_client
            .expect_get_node_info()
            .once()
            .returning(|_, _| Err(tonic_lnd::tonic::Status::not_found("Node not found")));

        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        let destination_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let destination_node = PublicKey::from_secret_key(&secp_ctx, &destination_secret);
        let destination = Destination::Node(destination_node);

        let peers = vec![]; // Empty peers, so it will try get_node_info

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_path_node_info_returns_none_for_node() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mut mock_client = MockTestPeerConnector::new();

        // Mock get_node_info to return NodeInfo with None node
        mock_client.expect_get_node_info().once().returning(|_, _| {
            Ok(tonic_lnd::lnrpc::NodeInfo {
                node: None,
                num_channels: 0,
                total_capacity: 0,
                channels: vec![],
            })
        });

        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        let destination_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let destination_node = PublicKey::from_secret_key(&secp_ctx, &destination_secret);
        let destination = Destination::Node(destination_node);

        let peers = vec![]; // Empty peers, so it will try get_node_info

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_path_node_without_addresses() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mut mock_client = MockTestPeerConnector::new();

        // Mock get_node_info to return node with empty addresses
        mock_client.expect_get_node_info().once().returning(|_, _| {
            Ok(tonic_lnd::lnrpc::NodeInfo {
                node: Some(tonic_lnd::lnrpc::LightningNode {
                    last_update: 0,
                    pub_key: "".to_string(),
                    alias: "".to_string(),
                    addresses: vec![], // Empty addresses
                    color: "".to_string(),
                    features: std::collections::HashMap::new(),
                    custom_records: std::collections::HashMap::new(),
                }),
                num_channels: 0,
                total_capacity: 0,
                channels: vec![],
            })
        });

        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        let destination_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let destination_node = PublicKey::from_secret_key(&secp_ctx, &destination_secret);
        let destination = Destination::Node(destination_node);

        let peers = vec![];

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_path_node_without_onion_message_features() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mut mock_client = MockTestPeerConnector::new();

        // Mock get_node_info to return node without onion message features
        mock_client.expect_get_node_info().once().returning(|_, _| {
            Ok(tonic_lnd::lnrpc::NodeInfo {
                node: Some(tonic_lnd::lnrpc::LightningNode {
                    last_update: 0,
                    pub_key: "".to_string(),
                    alias: "".to_string(),
                    addresses: vec![tonic_lnd::lnrpc::NodeAddress {
                        network: "tcp".to_string(),
                        addr: "127.0.0.1:9735".to_string(),
                    }],
                    color: "".to_string(),
                    features: std::collections::HashMap::new(), /* Empty features (no onion
                                                                 * message support) */
                    custom_records: std::collections::HashMap::new(),
                }),
                num_channels: 0,
                total_capacity: 0,
                channels: vec![],
            })
        });

        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        let destination_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let destination_node = PublicKey::from_secret_key(&secp_ctx, &destination_secret);
        let destination = Destination::Node(destination_node);

        let peers = vec![];

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_path_node_with_unparseable_addresses() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mut mock_client = MockTestPeerConnector::new();

        // Mock get_node_info to return node with onion message features but unparseable addresses
        mock_client.expect_get_node_info().once().returning(|_, _| {
            let mut features = std::collections::HashMap::new();
            // Add onion message feature (feature bit 38/39)
            features.insert(
                38,
                tonic_lnd::lnrpc::Feature {
                    name: "onion_messages_optional".to_string(),
                    is_required: false,
                    is_known: true,
                },
            );

            Ok(tonic_lnd::lnrpc::NodeInfo {
                node: Some(tonic_lnd::lnrpc::LightningNode {
                    last_update: 0,
                    pub_key: "".to_string(),
                    alias: "".to_string(),
                    addresses: vec![tonic_lnd::lnrpc::NodeAddress {
                        network: "tcp".to_string(),
                        addr: "invalid-address".to_string(), // Unparseable address
                    }],
                    color: "".to_string(),
                    features,
                    custom_records: std::collections::HashMap::new(),
                }),
                num_channels: 0,
                total_capacity: 0,
                channels: vec![],
            })
        });

        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        let destination_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let destination_node = PublicKey::from_secret_key(&secp_ctx, &destination_secret);
        let destination = Destination::Node(destination_node);

        let peers = vec![];

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_path_happy_path() {
        let mock_router = MockMessageRouter::new(Ok(vec![]));
        let mut mock_client = MockTestPeerConnector::new();

        // Mock get_node_info to return node with onion message features and parseable addresses
        mock_client.expect_get_node_info().once().returning(|_, _| {
            let mut features = std::collections::HashMap::new();
            // Add onion message feature (feature bit 38/39)
            features.insert(
                38,
                tonic_lnd::lnrpc::Feature {
                    name: "onion_messages_optional".to_string(),
                    is_required: false,
                    is_known: true,
                },
            );

            Ok(tonic_lnd::lnrpc::NodeInfo {
                node: Some(tonic_lnd::lnrpc::LightningNode {
                    last_update: 0,
                    pub_key: "".to_string(),
                    alias: "".to_string(),
                    addresses: vec![
                        tonic_lnd::lnrpc::NodeAddress {
                            network: "tcp".to_string(),
                            addr: "127.0.0.1:9735".to_string(),
                        },
                        tonic_lnd::lnrpc::NodeAddress {
                            network: "tcp".to_string(),
                            addr: "[::1]:9735".to_string(),
                        },
                    ],
                    color: "".to_string(),
                    features,
                    custom_records: std::collections::HashMap::new(),
                }),
                num_channels: 0,
                total_capacity: 0,
                channels: vec![],
            })
        });

        let message_router = MessageRouter::new(mock_router, mock_client);

        let secp_ctx = create_secp_ctx();

        let sender_secret = SecretKey::from_slice(&[3; 32]).unwrap();
        let sender = PublicKey::from_secret_key(&secp_ctx, &sender_secret);

        let destination_secret = SecretKey::from_slice(&[4; 32]).unwrap();
        let destination_node = PublicKey::from_secret_key(&secp_ctx, &destination_secret);
        let destination = Destination::Node(destination_node);

        let peers = vec![];

        let result = message_router.find_path(sender, peers, destination);
        assert!(result.is_ok());

        let path = result.unwrap();
        assert!(path.intermediate_nodes.is_empty());
        assert!(path.first_node_addresses.is_some());

        let addresses = path.first_node_addresses.unwrap();
        assert_eq!(addresses.len(), 2);
        assert!(addresses
            .iter()
            .any(|addr| addr.to_string() == "127.0.0.1:9735"));
        // IPv6 addresses get expanded, so check for the expanded form
        assert!(addresses
            .iter()
            .any(|addr| addr.to_string() == "[0000:0000:0000:0000:0000:0000:0000:0001]:9735"));
    }
}
