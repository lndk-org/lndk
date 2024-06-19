mod internal {
    #![allow(unused_imports)]
    #![allow(clippy::enum_variant_names)]
    #![allow(clippy::unnecessary_lazy_evaluations)]
    #![allow(clippy::useless_conversion)]
    #![allow(clippy::never_loop)]
    #![allow(clippy::uninlined_format_args)]

    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

use home::home_dir;
use internal::*;
use lndk::lnd::{get_lnd_client, validate_lnd_creds, LndCfg};
use lndk::server::{generate_tls_creds, read_tls, LNDKServer};
use lndk::{
    lndkrpc, setup_logger, Cfg, LifecycleSignals, LndkOnionMessenger, OfferHandler,
    DEFAULT_DATA_DIR, DEFAULT_SERVER_HOST, DEFAULT_SERVER_PORT,
};
use lndkrpc::offers_server::OffersServer;
use log::{error, info};
use std::fs::create_dir_all;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::select;
use tonic::transport::{Server, ServerTlsConfig};
use tonic_lnd::lnrpc::GetInfoRequest;

#[macro_use]
extern crate configure_me;

#[tokio::main]
async fn main() -> Result<(), ()> {
    let config = Config::including_optional_config_files(&["./lndk.conf"])
        .unwrap_or_exit()
        .0;

    let creds = validate_lnd_creds(
        config.cert_path,
        config.cert_pem,
        config.macaroon_path,
        config.macaroon_hex,
    )
    .map_err(|e| {
        println!("Error validating config: {e}.");
    })?;
    let address = config.address.clone();
    let lnd_args = LndCfg::new(config.address, creds.clone());

    let (shutdown, listener) = triggered::trigger();
    let signals = LifecycleSignals { shutdown, listener };
    let args = Cfg {
        lnd: lnd_args,
        signals,
    };

    let handler = Arc::new(OfferHandler::new());
    let messenger = LndkOnionMessenger::new();

    let data_dir =
        create_data_dir().map_err(|e| println!("Error creating LNDK's data dir {e:?}"))?;
    setup_logger(config.log_level, config.log_dir)?;

    let mut client = get_lnd_client(args.lnd.clone()).expect("failed to connect to lnd");
    let info = client
        .lightning()
        .get_info(GetInfoRequest {})
        .await
        .expect("failed to get info")
        .into_inner();

    let grpc_host = match config.grpc_host {
        Some(host) => host,
        None => DEFAULT_SERVER_HOST.to_string(),
    };
    let grpc_port = match config.grpc_port {
        Some(port) => port,
        None => DEFAULT_SERVER_PORT,
    };
    let addr = format!("{grpc_host}:{grpc_port}").parse().map_err(|e| {
        error!("Error parsing API address: {e}");
    })?;
    let lnd_tls_str = creds.get_certificate_string()?;

    // The user passed in a TLS cert to help us establish a secure connection to LND. But now we
    // need to generate a TLS credentials for connecting securely to the LNDK server.
    generate_tls_creds(data_dir.clone()).map_err(|e| {
        error!("Error generating tls credentials: {e}");
    })?;
    let identity = read_tls(data_dir).map_err(|e| {
        error!("Error reading tls credentials: {e}");
    })?;

    let server = LNDKServer::new(
        Arc::clone(&handler),
        &info.identity_pubkey,
        lnd_tls_str,
        address,
    )
    .await;

    let server_fut = Server::builder()
        .tls_config(ServerTlsConfig::new().identity(identity))
        .expect("couldn't configure tls")
        .add_service(OffersServer::new(server))
        .serve(addr);

    info!("Starting lndk's grpc server at address {grpc_host}:{grpc_port}");

    select! {
       _ = messenger.run(args, Arc::clone(&handler)) => {
           info!("Onion messenger completed");
       },
       result2 = server_fut => {
            match result2 {
                Ok(_) => info!("API completed"),
                Err(e) => error!("Error running API: {}", e),
            };
       },
    }

    Ok(())
}

// Creates lndk's data directory at ~/.lndk.
fn create_data_dir() -> Result<PathBuf, std::io::Error> {
    let path = home_dir().unwrap().join(DEFAULT_DATA_DIR);
    create_dir_all(&path)?;

    Ok(path)
}
