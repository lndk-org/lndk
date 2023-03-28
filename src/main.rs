use async_trait::async_trait;
use bitcoin::bech32::u5;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{self, PublicKey, Scalar, Secp256k1};
use futures::executor::block_on;
use lightning::chain::keysinterface::{EntropySource, KeyMaterial, NodeSigner, Recipient};
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::onion_message::OnionMessenger;
use lightning::util::logger::{Level, Logger, Record};
use log::{debug, error, info, trace, warn};
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use tonic_lnd::{
    lnrpc::GetInfoRequest, lnrpc::GetInfoResponse, tonic::Code, tonic::Status, Client,
    ConnectError, LightningClient, PeersClient,
};

const ONION_MESSAGES_OPTIONAL: u32 = 39;

#[tokio::main]
async fn main() -> Result<(), ()> {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let args = match parse_args() {
        Ok(args) => args,
        Err(args) => panic!("Bad arguments: {args}"),
    };

    let mut client = get_lnd_client(args).expect("failed to connect");

    let info = client
        .lightning()
        .get_info(GetInfoRequest {})
        .await
        .expect("failed to get info")
        .into_inner();

    let pubkey = PublicKey::from_str(&info.identity_pubkey).unwrap();
    info!("Starting lndk for node: {pubkey}");

    if !info.features.contains_key(&ONION_MESSAGES_OPTIONAL) {
        info!("Attempting to set onion messaging feature bit ({ONION_MESSAGES_OPTIONAL})");

        let mut node_info_retriever = GetInfoClient {
            client: &mut client.lightning().clone(),
        };
        let mut announcement_updater = UpdateNodeAnnClient {
            peers_client: client.peers(),
        };
        match set_feature_bit(&mut node_info_retriever, &mut announcement_updater).await {
            Ok(_) => {}
            Err(err) => {
                error!("Error setting feature bit: {err}");
                return Err(());
            }
        }
    }

    let node_signer = LndNodeSigner::new(pubkey, client.signer());
    let messenger_utils = MessengerUtilities::new();
    let _onion_messenger = OnionMessenger::new(
        &messenger_utils,
        &node_signer,
        &messenger_utils,
        IgnoringMessageHandler {},
    );

    Ok(())
}

struct LndNodeSigner<'a> {
    pubkey: PublicKey,
    secp_ctx: Secp256k1<secp256k1::All>,
    signer: RefCell<&'a mut tonic_lnd::SignerClient>,
}

impl<'a> LndNodeSigner<'a> {
    fn new(pubkey: PublicKey, signer: &'a mut tonic_lnd::SignerClient) -> Self {
        LndNodeSigner {
            pubkey,
            secp_ctx: Secp256k1::new(),
            signer: RefCell::new(signer),
        }
    }
}

impl<'a> NodeSigner for LndNodeSigner<'a> {
    /// Get node id based on the provided [`Recipient`].
    ///
    /// This method must return the same value each time it is called with a given [`Recipient`]
    /// parameter.
    ///
    /// Errors if the [`Recipient`] variant is not supported by the implementation.
    fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
        match recipient {
            Recipient::Node => Ok(self.pubkey),
            Recipient::PhantomNode => Err(()),
        }
    }

    /// Gets the ECDH shared secret of our node secret and `other_key`, multiplying by `tweak` if
    /// one is provided. Note that this tweak can be applied to `other_key` instead of our node
    /// secret, though this is less efficient.
    ///
    /// Errors if the [`Recipient`] variant is not supported by the implementation.
    fn ecdh(
        &self,
        recipient: Recipient,
        other_key: &PublicKey,
        tweak: Option<&Scalar>,
    ) -> Result<SharedSecret, ()> {
        match recipient {
            Recipient::Node => {}
            Recipient::PhantomNode => return Err(()),
        }

        // Clone other_key so that we can tweak it (if a tweak is required). We choose to tweak the
        // `other_key` because LND's API accept a tweak parameter (so we can't tweak our secret).
        let tweaked_key = if let Some(tweak) = tweak {
            other_key.mul_tweak(&self.secp_ctx, tweak).map_err(|_| ())?
        } else {
            *other_key
        };

        let shared_secret = match block_on(self.signer.borrow_mut().derive_shared_key(
            tonic_lnd::signrpc::SharedKeyRequest {
                ephemeral_pubkey: tweaked_key.serialize().into_iter().collect::<Vec<u8>>(),
                key_desc: None,
                ..Default::default()
            },
        )) {
            Ok(shared_key_resp) => shared_key_resp.into_inner().shared_key,
            Err(_) => return Err(()),
        };

        match SharedSecret::from_slice(&shared_secret) {
            Ok(secret) => Ok(secret),
            Err(_) => Err(()),
        }
    }

    fn get_inbound_payment_key_material(&self) -> KeyMaterial {
        unimplemented!("not required for onion messaging");
    }

    fn sign_invoice(
        &self,
        _hrp_bytes: &[u8],
        _invoice_data: &[u5],
        _recipient: Recipient,
    ) -> Result<RecoverableSignature, ()> {
        unimplemented!("not required for onion messaging");
    }

    fn sign_gossip_message(&self, _msg: UnsignedGossipMessage) -> Result<Signature, ()> {
        unimplemented!("not required for onion messaging");
    }
}

// MessengerUtilities implements some utilites required for onion messenging.
struct MessengerUtilities {
    entropy_source: RefCell<ChaCha20Rng>,
}

impl MessengerUtilities {
    fn new() -> Self {
        MessengerUtilities {
            entropy_source: RefCell::new(ChaCha20Rng::from_entropy()),
        }
    }
}

impl EntropySource for MessengerUtilities {
    // TODO: surface LDK's EntropySource and use instead.
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        let mut chacha_bytes: [u8; 32] = [0; 32];
        self.entropy_source
            .borrow_mut()
            .fill_bytes(&mut chacha_bytes);
        chacha_bytes
    }
}

impl Logger for MessengerUtilities {
    fn log(&self, record: &Record) {
        let args_str = record.args.to_string();
        match record.level {
            Level::Gossip => {}
            Level::Trace => trace!("{}", args_str),
            Level::Debug => debug!("{}", args_str),
            Level::Info => info!("{}", args_str),
            Level::Warn => warn!("{}", args_str),
            Level::Error => error!("{}", args_str),
        }
    }
}

fn get_lnd_client(cfg: LndCfg) -> Result<Client, ConnectError> {
    block_on(tonic_lnd::connect(cfg.address, cfg.cert, cfg.macaroon))
}

#[derive(Debug)]
enum ArgsError {
    NoArgs,
    AddressRequired,
    CertRequired,
    MacaroonRequired,
}

impl Error for ArgsError {}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ArgsError::NoArgs => write!(f, "No command line arguments provided."),
            ArgsError::AddressRequired => write!(f, "LND's RPC server address is required."),
            ArgsError::CertRequired => write!(f, "Path to LND's tls certificate is required."),
            ArgsError::MacaroonRequired => write!(f, "Path to LND's macaroon is required."),
        }
    }
}

struct LndCfg {
    address: String,
    cert: String,
    macaroon: String,
}

impl LndCfg {
    fn new(address: String, cert: String, macaroon: String) -> LndCfg {
        LndCfg {
            address,
            cert,
            macaroon,
        }
    }
}

fn parse_args() -> Result<LndCfg, ArgsError> {
    let mut args = std::env::args_os();
    if args.next().is_none() {
        return Err(ArgsError::NoArgs);
    }

    let address = match args.next() {
        Some(arg) => arg.into_string().expect("address is not UTF-8"),
        None => return Err(ArgsError::AddressRequired),
    };

    let cert_file = match args.next() {
        Some(arg) => arg.into_string().expect("cert is not UTF-8"),
        None => return Err(ArgsError::CertRequired),
    };

    let macaroon_file = match args.next() {
        Some(arg) => arg.into_string().expect("macaroon is not UTF-8"),
        None => return Err(ArgsError::MacaroonRequired),
    };

    Ok(LndCfg::new(address, cert_file, macaroon_file))
}

#[derive(Debug)]
enum SetOnionBitError {
    UnimplementedPeersService,
    SetBitFail,
    UpdateAnnouncementErr(Status),
    GetInfoErr(Status),
}

impl Error for SetOnionBitError {}

impl fmt::Display for SetOnionBitError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SetOnionBitError::UnimplementedPeersService => write!(
                f,
                "peer service is unimplemented, remember to
                enable the peerrpc services when building LND with make tags='peerrpc signrpc'"
            ),
            SetOnionBitError::SetBitFail => {
                write!(f, "onion messaging feature bit failed to be set")
            }
            SetOnionBitError::UpdateAnnouncementErr(err) => {
                write!(f, "error setting update announcement: {err}")
            }
            SetOnionBitError::GetInfoErr(err) => write!(f, "get_info error: {err}"),
        }
    }
}

#[async_trait]
trait NodeInfoRetriever {
    async fn get_info(
        &mut self,
        request: GetInfoRequest,
    ) -> Result<GetInfoResponse, SetOnionBitError>;
}

struct GetInfoClient<'a> {
    client: &'a mut LightningClient,
}

#[async_trait]
impl<'a> NodeInfoRetriever for GetInfoClient<'a> {
    async fn get_info(
        &mut self,
        request: GetInfoRequest,
    ) -> Result<GetInfoResponse, SetOnionBitError> {
        match self.client.get_info(request).await {
            Ok(resp) => Ok(resp.into_inner()),
            Err(status) => Err(SetOnionBitError::GetInfoErr(status)),
        }
    }
}

/// UpdateNodeAnnouncement defines the peer functionality needed to set the onion
/// messaging feature bit.
#[async_trait]
trait UpdateNodeAnnouncement {
    async fn update_node_announcement(
        &mut self,
        request: tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest,
    ) -> Result<(), SetOnionBitError>;
}

struct UpdateNodeAnnClient<'a> {
    peers_client: &'a mut PeersClient,
}

#[async_trait]
impl<'a> UpdateNodeAnnouncement for UpdateNodeAnnClient<'a> {
    async fn update_node_announcement(
        &mut self,
        request: tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest,
    ) -> Result<(), SetOnionBitError> {
        match self.peers_client.update_node_announcement(request).await {
            Ok(_) => Ok(()),
            Err(status) => {
                if status.code() == Code::Unimplemented {
                    return Err(SetOnionBitError::UnimplementedPeersService);
                }

                return Err(SetOnionBitError::UpdateAnnouncementErr(status));
            }
        }
    }
}

/// Sets the onion messaging feature bit (described in this PR:
/// https://github.com/lightning/bolts/pull/759/), to signal that we support
/// onion messaging. This needs to be done every time LND starts up, because LND
/// does not currently persist the custom feature bits that are set via the RPC.
async fn set_feature_bit(
    client: &mut impl NodeInfoRetriever,
    peers_client: &mut impl UpdateNodeAnnouncement,
) -> Result<(), SetOnionBitError> {
    let feature_updates = vec![tonic_lnd::peersrpc::UpdateFeatureAction {
        action: i32::from(tonic_lnd::peersrpc::UpdateAction::Add),
        feature_bit: ONION_MESSAGES_OPTIONAL as i32,
    }];

    peers_client
        .update_node_announcement(tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest {
            feature_updates,
            ..Default::default()
        })
        .await?;

    // Call get_info again to check the bit was actually set.
    let info = client.get_info(GetInfoRequest {}).await?;

    if !info.features.contains_key(&ONION_MESSAGES_OPTIONAL) {
        return Err(SetOnionBitError::SetBitFail);
    }

    info!("Successfully set onion messaging bit");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::mock;
    use std::collections::HashMap;

    mock! {
        InfoRetriever{}

        #[async_trait]
        impl NodeInfoRetriever for InfoRetriever{
            async fn get_info(
                &mut self, request: tonic_lnd::lnrpc::GetInfoRequest,
            ) -> Result<tonic_lnd::lnrpc::GetInfoResponse, SetOnionBitError>;
        }
    }

    mock! {
        NodeAnnouncementUpdater{}

        #[async_trait]
        impl UpdateNodeAnnouncement for NodeAnnouncementUpdater{
            async fn update_node_announcement(
                &mut self, request: tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest,
            ) -> Result<(), SetOnionBitError>;
        }
    }

    #[tokio::test]
    async fn test_set_feature_bit_success() {
        let mut client_mock = MockInfoRetriever::new();
        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();

        // Let's first test that when the peer & main clients return the values
        // we need, set_feature_bit works ok.
        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| Ok(()));

        client_mock.expect_get_info().returning(|_| {
            Ok(GetInfoResponse {
                features: HashMap::from([(
                    ONION_MESSAGES_OPTIONAL,
                    tonic_lnd::lnrpc::Feature {
                        name: String::from("onion_message"),
                        is_known: true,
                        is_required: false,
                    },
                )]),
                ..Default::default()
            })
        });

        let set_feature_bit = set_feature_bit(&mut client_mock, &mut peers_client_mock).await;

        matches!(set_feature_bit, Ok(()));
    }

    #[tokio::test]
    async fn test_update_node_announcement_failure() {
        let mut client_mock = MockInfoRetriever::new();
        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();

        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| {
                Err(SetOnionBitError::UpdateAnnouncementErr(Status::new(
                    Code::Unavailable,
                    "",
                )))
            });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error");

        // If the peers client returns a Status error, set_feature_bit should
        // return that error.
        matches!(set_feature_err, SetOnionBitError::UpdateAnnouncementErr(_));

        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();
        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| {
                Err(SetOnionBitError::UpdateAnnouncementErr(Status::new(
                    Code::Unimplemented,
                    "",
                )))
            });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error with unavailable");

        // If the peers client returns Code::Unimplemented, we should get
        // the correct error message.
        matches!(set_feature_err, SetOnionBitError::UnimplementedPeersService);
    }

    #[tokio::test]
    async fn test_check_if_onion_message_set_failure() {
        let mut client_mock = MockInfoRetriever::new();
        let mut peers_client_mock = MockNodeAnnouncementUpdater::new();

        peers_client_mock
            .expect_update_node_announcement()
            .returning(|_| Ok(()));

        client_mock.expect_get_info().returning(|_| {
            Err(SetOnionBitError::UpdateAnnouncementErr(Status::new(
                Code::Unavailable,
                "",
            )))
        });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error with unavailable");

        matches!(set_feature_err, SetOnionBitError::UpdateAnnouncementErr(_));

        // If get_info returns a response with the wrong feature bit set,
        // set_feature_bit should throw another error.
        client_mock.expect_get_info().returning(|_| {
            Ok(GetInfoResponse {
                features: HashMap::from([(
                    8,
                    tonic_lnd::lnrpc::Feature {
                        name: String::from("testing"),
                        is_known: true,
                        is_required: false,
                    },
                )]),
                ..Default::default()
            })
        });

        let set_feature_err = set_feature_bit(&mut client_mock, &mut peers_client_mock)
            .await
            .expect_err("set_feature_bit should error");

        matches!(set_feature_err, SetOnionBitError::SetBitFail);
    }
}
