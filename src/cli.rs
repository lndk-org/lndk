use clap::{Parser, Subcommand};
use lndk::lndk_offers::decode;

/// A cli for interacting with lndk.
#[derive(Debug, Parser)]
#[command(name = "lndk-cli")]
#[command(about = "A cli for interacting with lndk", long_about = None)]
struct Cli {
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
