#[macro_use]
extern crate configure_me;

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
use std::str::FromStr;
use tonic_lnd::{Client, ConnectError};

include_config!();

#[tokio::main]
async fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let lnd_config = Config::including_optional_config_files(&[""])
        .unwrap_or_exit()
        .0;
    let args = LndCfg::new(lnd_config.address, lnd_config.cert, lnd_config.macaroon);
    let mut client = get_lnd_client(args).expect("failed to connect");

    let info = client
        .lightning()
        .get_info(tonic_lnd::lnrpc::GetInfoRequest {})
        .await
        .expect("failed to get info");

    let pubkey = PublicKey::from_str(&info.into_inner().identity_pubkey).unwrap();
    info!("Starting lndk for node: {pubkey}");

    let node_signer = LndNodeSigner::new(pubkey, client.signer());
    let messenger_utils = MessengerUtilities::new();
    let _onion_messenger = OnionMessenger::new(
        &messenger_utils,
        &node_signer,
        &messenger_utils,
        IgnoringMessageHandler {},
    );
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
