use clap::{Parser, Subcommand};
use lightning::offers::invoice::Bolt12Invoice;
use lndk::lndk_offers::decode;
use lndk::lndkrpc::offers_client::OffersClient;
use lndk::lndkrpc::{GetInvoiceRequest, PayInvoiceRequest, PayOfferRequest};
use lndk::{
    Bolt12InvoiceString, DEFAULT_DATA_DIR, DEFAULT_SERVER_HOST, DEFAULT_SERVER_PORT,
    TLS_CERT_FILENAME,
};
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::path::PathBuf;
use std::process::exit;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
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

    /// This option is for passing a pem-encoded TLS certificate string to establish a connection
    /// with the LNDK server. If this isn't set, the cli will look for the TLS file in the default
    /// location (~.lndk).
    #[arg(long, global = true, required = false)]
    cert_pem: Option<String>,

    #[arg(long, global = true, required = false, default_value = format!("https://{DEFAULT_SERVER_HOST}"))]
    grpc_host: String,

    #[arg(long, global = true, required = false, default_value = DEFAULT_SERVER_PORT.to_string())]
    grpc_port: u16,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Decodes a bech32-encoded offer string into a BOLT 12 offer.
    DecodeOffer {
        /// The offer string to decode.
        offer_string: String,
    },
    /// Decodes a bech32-encoded invoice string into a BOLT 12 invoice.
    DecodeInvoice {
        /// The invoice string to decode.
        invoice_string: String,
    },
    /// PayOffer pays a BOLT 12 offer, provided as a 'lno'-prefaced offer string.
    PayOffer {
        /// The offer string.
        offer_string: String,

        /// Amount the user would like to pay. If this isn't set, we'll assume the user is paying
        /// whatever the offer amount is.
        #[arg(required = false)]
        amount: Option<u64>,

        /// A payer-provided note which will be seen by the recipient.
        #[arg(required = false)]
        payer_note: Option<String>,
    },
    /// GetInvoice fetch a BOLT 12 invoice, which will be returned as a hex-encoded string. It
    /// fetches the invoice from a BOLT 12 offer, provided as a 'lno'-prefaced offer string.
    GetInvoice {
        /// The offer string.
        offer_string: String,

        /// Amount the user would like to pay. If this isn't set, we'll assume the user is paying
        /// whatever the offer amount is.
        #[arg(required = false)]
        amount: Option<u64>,

        /// A payer-provided note which will be seen by the recipient.
        #[arg(required = false)]
        payer_note: Option<String>,
    },
    /// PayInvoice pays a hex-encoded BOLT12 invoice.
    PayInvoice {
        /// The hex-encoded invoice string.
        invoice_string: String,
        /// Amount the user would like to pay. If this isn't set, we'll assume the user is paying
        /// whatever the invoice amount is set to.
        #[arg(required = false)]
        amount: Option<u64>,
    },
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    match args.command {
        Commands::DecodeOffer { offer_string } => {
            println!("Decoding offer: {offer_string}.");
            match decode(offer_string) {
                Ok(offer) => {
                    println!("Decoded offer: {:?}.", offer)
                }
                Err(e) => {
                    println!(
                        "ERROR please provide offer starting with lno. Provided offer is \
                        invalid, failed to decode with error: {:?}.",
                        e
                    );
                    exit(1)
                }
            }
        }
        Commands::DecodeInvoice { invoice_string } => {
            println!("Decoding invoice: {invoice_string}.");

            let invoice_string: Bolt12InvoiceString = invoice_string.clone().into();
            match Bolt12Invoice::try_from(invoice_string) {
                Ok(invoice) => {
                    println!("Decoded invoice: {:?}.", invoice);
                }
                Err(e) => {
                    println!(
                        "ERROR please provide hex-encoded invoice string. Provided invoice is \
                        invalid, failed to decode with error: {:?}.",
                        e
                    );
                    exit(1);
                }
            }
        }
        Commands::PayOffer {
            ref offer_string,
            amount,
            payer_note,
        } => {
            let tls = read_cert_from_args(args.cert_pem);
            let grpc_host = args.grpc_host;
            let grpc_port = args.grpc_port;
            let channel = Channel::from_shared(format!("{grpc_host}:{grpc_port}"))
                .unwrap_or_else(|e| {
                    println!("ERROR creating endpoint: {e:?}");
                    exit(1)
                })
                .tls_config(tls)
                .unwrap_or_else(|e| {
                    println!("ERROR tls config: {e:?}");
                    exit(1)
                })
                .connect()
                .await
                .unwrap_or_else(|e| {
                    println!("ERROR connecting: {e:?}");
                    exit(1)
                });

            let mut client = OffersClient::new(channel);

            let offer = match decode(offer_string.to_owned()) {
                Ok(offer) => offer,
                Err(e) => {
                    println!(
                        "ERROR: please provide offer starting with lno. Provided offer is \
                        invalid, failed to decode with error: {:?}.",
                        e
                    );
                    exit(1)
                }
            };

            let macaroon =
                read_macaroon_from_args(args.macaroon_path, args.macaroon_hex, &args.network);
            let mut request = Request::new(PayOfferRequest {
                offer: offer.to_string(),
                amount,
                payer_note,
            });
            add_metadata(&mut request, macaroon).unwrap_or_else(|_| exit(1));

            match client.pay_offer(request).await {
                Ok(_) => println!("Successfully paid for offer!"),
                Err(err) => {
                    println!("Error paying for offer: {err:?}");
                    exit(1)
                }
            };
        }
        Commands::GetInvoice {
            ref offer_string,
            amount,
            payer_note,
        } => {
            let tls = read_cert_from_args(args.cert_pem);
            let grpc_host = args.grpc_host;
            let grpc_port = args.grpc_port;
            let channel = Channel::from_shared(format!("{grpc_host}:{grpc_port}"))
                .unwrap_or_else(|e| {
                    println!("ERROR creating endpoint: {e:?}");
                    exit(1)
                })
                .tls_config(tls)
                .unwrap_or_else(|e| {
                    println!("ERROR tls config: {e:?}");
                    exit(1)
                })
                .connect()
                .await
                .unwrap_or_else(|e| {
                    println!("ERROR connecting: {e:?}");
                    exit(1)
                });

            let mut client = OffersClient::new(channel);
            let offer = match decode(offer_string.to_owned()) {
                Ok(offer) => offer,
                Err(e) => {
                    println!(
                        "ERROR: please provide offer starting with lno. Provided offer is \
                        invalid, failed to decode with error: {:?}.",
                        e
                    );
                    exit(1)
                }
            };

            let macaroon =
                read_macaroon_from_args(args.macaroon_path, args.macaroon_hex, &args.network);
            let mut request = Request::new(GetInvoiceRequest {
                offer: offer.to_string(),
                amount,
                payer_note,
            });
            add_metadata(&mut request, macaroon).unwrap_or_else(|_| exit(1));
            match client.get_invoice(request).await {
                Ok(response) => {
                    println!("Invoice: {:?}.", response.get_ref())
                }
                Err(err) => {
                    println!("Error getting invoice for offer: {err:?}");
                    exit(1)
                }
            }
        }
        Commands::PayInvoice {
            ref invoice_string,
            amount,
        } => {
            let tls = read_cert_from_args(args.cert_pem.clone());
            let grpc_host = args.grpc_host.clone();
            let grpc_port = args.grpc_port;
            let channel = Channel::from_shared(format!("{grpc_host}:{grpc_port}"))
                .unwrap_or_else(|e| {
                    println!("ERROR creating endpoint: {e:?}");
                    exit(1)
                })
                .tls_config(tls)
                .unwrap_or_else(|e| {
                    println!("ERROR tls config: {e:?}");
                    exit(1)
                })
                .connect()
                .await
                .unwrap_or_else(|e| {
                    println!("ERROR connecting: {e:?}");
                    exit(1)
                });

            let mut client = OffersClient::new(channel);
            let macaroon =
                read_macaroon_from_args(args.macaroon_path, args.macaroon_hex, &args.network);
            let mut request = Request::new(PayInvoiceRequest {
                invoice: invoice_string.to_owned(),
                amount,
            });
            add_metadata(&mut request, macaroon).unwrap_or_else(|_| exit(1));
            match client.pay_invoice(request).await {
                Ok(_) => println!("Successfully paid for offer!"),
                Err(err) => {
                    println!("Error paying invoice: {err:?}");
                    exit(1)
                }
            }
        }
    }
}

fn add_metadata<R>(request: &mut Request<R>, macaroon: String) -> Result<(), ()> {
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

fn read_cert_from_args(cert_pem: Option<String>) -> ClientTlsConfig {
    let data_dir = home::home_dir().unwrap().join(DEFAULT_DATA_DIR);
    let pem = match &cert_pem {
        Some(pem) => pem.clone(),
        None => {
            // If no cert pem string is provided, we'll look for the tls certificate in the
            // default location.
            std::fs::read_to_string(data_dir.join(TLS_CERT_FILENAME)).unwrap_or_else(|e| {
                println!("ERROR reading cert: {e:?}");
                exit(1)
            })
        }
    };
    let cert = Certificate::from_pem(pem);
    ClientTlsConfig::new()
        .ca_certificate(cert)
        .domain_name("localhost")
}

fn read_macaroon_from_args(
    macaroon_path: Option<PathBuf>,
    macaroon_hex: Option<String>,
    network: &str,
) -> String {
    // Make sure both macaroon options are not set.
    if macaroon_path.is_some() && macaroon_hex.is_some() {
        println!("ERROR: Only one of `macaroon_path` or `macaroon_hex` should be set.");
        exit(1)
    }

    // Let's grab the macaroon string now. If neither macaroon_path nor macaroon_hex are
    // set, use the default macaroon path.
    match macaroon_path {
        Some(path) => read_macaroon_from_file(path.clone()).unwrap_or_else(|e| {
            println!("ERROR reading macaroon from file {e:?}");
            exit(1)
        }),
        None => match &macaroon_hex {
            Some(macaroon) => macaroon.clone(),
            None => {
                let path = get_macaroon_path_default(network);
                read_macaroon_from_file(path).unwrap_or_else(|e| {
                    println!("ERROR reading macaroon from file {e:?}");
                    exit(1)
                })
            }
        },
    }
}
