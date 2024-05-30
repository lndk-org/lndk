use clap::{Parser, Subcommand};
use lndk::lndk_offers::decode;
use lndk::lndkrpc::offers_client::OffersClient;
use lndk::lndkrpc::PayOfferRequest;
use lndk::{DEFAULT_SERVER_HOST, DEFAULT_SERVER_PORT};
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::path::PathBuf;
use tonic::Request;

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
    macaroon_path: Option<PathBuf>,

    /// A hex-encoded macaroon string to pass in directly to the cli.
    #[arg(long, global = true, required = false)]
    macaroon_hex: Option<String>,

    #[arg(long, global = true, required = false, default_value = format!("http://{DEFAULT_SERVER_HOST}"))]
    grpc_host: String,

    #[arg(long, global = true, required = false, default_value = DEFAULT_SERVER_PORT.to_string())]
    grpc_port: u16,

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
            ref offer_string,
            amount,
        } => {
            let grpc_host = args.grpc_host;
            let grpc_port = args.grpc_port;
            let mut client = OffersClient::connect(format!("{grpc_host}:{grpc_port}"))
                .await
                .map_err(|e| {
                    println!("ERROR: connecting to server {:?}.", e);
                })?;

            let offer = match decode(offer_string.to_owned()) {
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

            // Make sure both macaroon options are not set.
            if args.macaroon_path.is_some() && args.macaroon_hex.is_some() {
                println!("ERROR: Only one of `macaroon_path` or `macaroon_hex` should be set.");
                return Err(());
            }

            // Let's grab the macaroon string now. If neither macaroon_path nor macaroon_hex are
            // set, use the default macaroon path.
            let macaroon = match args.macaroon_path {
                Some(path) => read_macaroon_from_file(path)
                    .map_err(|e| println!("ERROR reading macaroon from file {e:?}"))?,
                None => match args.macaroon_hex {
                    Some(macaroon) => macaroon,
                    None => {
                        let path = get_macaroon_path_default(&args.network);
                        read_macaroon_from_file(path)
                            .map_err(|e| println!("ERROR reading macaroon from file {e:?}"))?
                    }
                },
            };

            let mut request = Request::new(PayOfferRequest {
                offer: offer.to_string(),
                amount,
            });
            add_metadata(&mut request, macaroon).map_err(|_| ())?;

            match client.pay_offer(request).await {
                Ok(_) => println!("Successfully paid for offer!"),
                Err(err) => println!("Error paying for offer: {err:?}"),
            };

            Ok(())
        }
    }
}

fn add_metadata(request: &mut Request<PayOfferRequest>, macaroon: String) -> Result<(), ()> {
    let macaroon = macaroon.parse().map_err(|e| {
        println!("Error parsing provided macaroon string into tonic metadata {e:?}")
    })?;
    request.metadata_mut().insert("macaroon", macaroon);

    Ok(())
}

fn read_macaroon_from_file(path: PathBuf) -> Result<String, std::io::Error> {
    let file = File::open(path)?;
    let mut mac_contents = BufReader::new(file);
    let mut buffer = Vec::new();
    mac_contents.read_to_end(&mut buffer)?;

    Ok(hex::encode(buffer))
}
