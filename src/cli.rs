use clap::{Parser, Subcommand};
use lndk::lnd::{get_lnd_client, string_to_network, LndCfg};
use lndk::lndk_offers::{decode, get_destination};
use lndk::{Cfg, LifecycleSignals, LndkOnionMessenger, OfferHandler, PayOfferParams};
use log::LevelFilter;
use std::ffi::OsString;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

fn get_cert_path_default() -> OsString {
    home::home_dir()
        .unwrap()
        .as_path()
        .join(".lnd")
        .join("tls.cert")
        .into_os_string()
}

fn get_macaroon_path_default() -> OsString {
    home::home_dir()
        .unwrap()
        .as_path()
        .join(".lnd/data/chain/bitcoin/regtest/admin.macaroon")
        .into_os_string()
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

    #[arg(short, long, global = true, required = false, default_value = get_cert_path_default())]
    tls_cert: String,

    #[arg(short, long, global = true, required = false, default_value = get_macaroon_path_default())]
    macaroon: String,

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
            let network = string_to_network(&args.network).map_err(|e| {
                println!("ERROR: invalid network string: {}", e);
            })?;
            let lnd_cfg = LndCfg::new(args.address, args.tls_cert.into(), args.macaroon.into());
            let client = get_lnd_client(lnd_cfg.clone()).map_err(|e| {
                println!("ERROR: failed to connect to lnd: {}", e);
            })?;

            let (shutdown, listener) = triggered::trigger();
            let (tx, rx): (Sender<u32>, Receiver<u32>) = mpsc::channel(1);
            let signals = LifecycleSignals {
                shutdown: shutdown.clone(),
                listener,
                started: tx,
            };
            let lndk_cfg = Cfg {
                lnd: lnd_cfg,
                log_dir: None,
                log_level: LevelFilter::Info,
                signals,
            };

            let handler = OfferHandler::new();
            let messenger = LndkOnionMessenger::new(handler);
            let pay_cfg = PayOfferParams {
                offer: offer.clone(),
                amount,
                network,
                client,
                destination,
                reply_path: None,
            };
            select! {
                _ = messenger.run(lndk_cfg) => {
                    println!("ERROR: lndk stopped running before pay offer finished.");
                },
                res = messenger.offer_handler.pay_offer(pay_cfg, rx) => {
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
