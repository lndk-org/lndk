use clap::{Parser, Subcommand};
use lndk::lnd::{get_lnd_client, string_to_network, validate_lnd_creds, LndCfg};
use lndk::lndk_offers::{decode, get_destination};
use lndk::{Cfg, LifecycleSignals, LndkOnionMessenger, OfferHandler, PayOfferParams};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::select;

fn get_cert_path_default() -> PathBuf {
    home::home_dir()
        .unwrap()
        .as_path()
        .join(".lnd")
        .join("tls.cert")
}

fn get_macaroon_path_default(network: &str) -> PathBuf {
    home::home_dir()
        .unwrap()
        .as_path()
        .join(format!(".lnd/data/chain/bitcoin/{network}/admin.macaroon"))
}

/// A cli for interacting with lndk.
#[derive(Debug, Parser)]
#[command(name = "lndk-cli")]
#[command(about = "A cli for interacting with lndk", long_about = None)]
struct Cli {
    /// Global variables
    #[arg(
        short,
        long,
        global = true,
        required = false,
        default_value = "regtest"
    )]
    network: String,

    #[arg(short, long, global = true, required = false)]
    cert_path: Option<PathBuf>,

    #[arg(short, long, global = true, required = false)]
    macaroon_path: Option<PathBuf>,

    /// A pem-encoded tls certificate to pass in directly to the cli. If cert_pem is set,
    /// macaroon_hex must also be set.
    #[arg(long, global = true, required = false)]
    cert_pem: Option<String>,

    /// A hex-encoded macaroon string to pass in directly to the cli. If macaroon_hex is set,
    /// cert_pem also must also be set.
    #[arg(long, global = true, required = false)]
    macaroon_hex: Option<String>,

    #[arg(
        short,
        long,
        global = true,
        required = false,
        default_value = "https://localhost:10009"
    )]
    address: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Decodes a bech32-encoded offer string into a BOLT 12 offer.
    Decode {
        /// The offer string to decode.
        offer_string: String,
    },
    /// PayOffer pays a BOLT 12 offer, provided as a 'lno'-prefaced offer string.
    PayOffer {
        /// The offer string.
        offer_string: String,

        /// Amount the user would like to pay. If this isn't set, we'll assume the user is paying
        /// whatever the offer amount is.
        #[arg(required = false)]
        amount: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<(), ()> {
    let args = Cli::parse();
    match args.command {
        Commands::Decode { offer_string } => {
            println!("Decoding offer: {offer_string}.");
            match decode(offer_string) {
                Ok(offer) => {
                    println!("Decoded offer: {:?}.", offer);
                    Ok(())
                }
                Err(e) => {
                    println!(
                        "ERROR please provide offer starting with lno. Provided offer is \
                        invalid, failed to decode with error: {:?}.",
                        e
                    );
                    Err(())
                }
            }
        }
        Commands::PayOffer {
            offer_string,
            amount,
        } => {
            let offer = match decode(offer_string) {
                Ok(offer) => offer,
                Err(e) => {
                    println!(
                        "ERROR: please provide offer starting with lno. Provided offer is \
                        invalid, failed to decode with error: {:?}.",
                        e
                    );
                    return Err(());
                }
            };

            let destination = get_destination(&offer).await;
            // If macaroon_path, macaroon_hex, and cert_pem are not set, use the default macaroon
            // path.
            let macaroon_path = match args.macaroon_path {
                Some(path) => Some(path),
                None => match args.macaroon_hex {
                    Some(_) => None,
                    None => match args.cert_pem {
                        Some(_) => None,
                        None => Some(get_macaroon_path_default(&args.network)),
                    },
                },
            };
            // If cert_path, cert_pem, and macaroon_hex are not set, use the default cert path.
            let cert_path = match args.cert_path {
                Some(path) => Some(path),
                None => match args.cert_pem {
                    Some(_) => None,
                    None => match args.macaroon_hex {
                        Some(_) => None,
                        None => Some(get_cert_path_default()),
                    },
                },
            };

            let network = string_to_network(&args.network).map_err(|e| {
                println!("ERROR: invalid network string: {}", e);
            })?;

            let creds =
                validate_lnd_creds(cert_path, args.cert_pem, macaroon_path, args.macaroon_hex)
                    .map_err(|e| {
                        println!("ERROR with user-provided credentials: {}", e);
                    })?;
            let lnd_cfg = LndCfg::new(args.address, creds);

            let client = get_lnd_client(lnd_cfg.clone()).map_err(|e| {
                println!("ERROR: failed to connect to lnd: {}", e);
            })?;

            let (shutdown, listener) = triggered::trigger();
            let signals = LifecycleSignals {
                shutdown: shutdown.clone(),
                listener,
            };
            let lndk_cfg = Cfg {
                lnd: lnd_cfg,
                signals,
            };

            let handler = Arc::new(OfferHandler::new());
            let messenger = LndkOnionMessenger::new();
            let pay_cfg = PayOfferParams {
                offer: offer.clone(),
                amount,
                network,
                client,
                destination,
                reply_path: None,
            };
            select! {
                _ = messenger.run(lndk_cfg, Arc::clone(&handler)) => {
                    println!("ERROR: lndk stopped running before pay offer finished.");
                },
                res = handler.pay_offer(pay_cfg) => {
                    match res {
                        Ok(_) => println!("Successfully paid for offer!"),
                        Err(err) => println!("Error paying for offer: {err:?}"),
                    }

                    shutdown.trigger();
                }
            }

            Ok(())
        }
    }
}
