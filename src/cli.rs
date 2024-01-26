use clap::{Parser, Subcommand};
use lndk::lndk_offers::decode;
use std::ffi::OsString;

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
}

fn main() {
    let args = Cli::parse();
    match args.command {
        Commands::Decode { offer_string } => {
            println!("Decoding offer: {offer_string}.");
            match decode(offer_string) {
                Ok(offer) => println!("Decoded offer: {:?}.", offer),
                Err(e) => {
                    println!(
                        "ERROR please provide offer starting with lno. Provided offer is \
                        invalid, failed to decode with error: {:?}.",
                        e
                    )
                }
            }
        }
    }
}
