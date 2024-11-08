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
use lndk::lnd::{validate_lnd_creds, LndCfg};
use lndk::server::setup_server;
use lndk::{
    setup_logger, Cfg, LifecycleSignals, LndkOnionMessenger, OfferHandler, DEFAULT_DATA_DIR,
};
use log::{error, info};
use std::fs::create_dir_all;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use tokio::select;
use tokio::signal::unix::SignalKind;

#[macro_use]
extern crate configure_me;

#[tokio::main]
async fn main() -> Result<(), ()> {
    let config = Config::including_optional_config_files(&["./lndk.conf"])
        .unwrap_or_exit()
        .0;

    let data_dir =
        create_data_dir().map_err(|e| println!("Error creating LNDK's data dir {e:?}"))?;
    setup_logger(config.log_level, config.log_dir)?;

    let creds = validate_lnd_creds(
        config.cert_path,
        config.cert_pem,
        config.macaroon_path,
        config.macaroon_hex,
    )
    .map_err(|e| {
        error!("Error validating config: {e}.");
    })?;
    let address = config.address.clone();
    let lnd_args = LndCfg::new(config.address, creds.clone());

    let (shutdown, listener) = triggered::trigger();
    let signals = LifecycleSignals {
        shutdown: shutdown.clone(),
        listener: listener.clone(),
    };
    let args = Cfg {
        lnd: lnd_args,
        signals,
        skip_version_check: config.skip_version_check,
        rate_limit_count: config.rate_limit_count,
        rate_limit_period_secs: config.rate_limit_period_secs,
    };

    let mut sigterm_stream = tokio::signal::unix::signal(SignalKind::terminate())
        .map_err(|e| error!("Error initializing sigterm signal: {e}."))?;
    let mut sigint_stream = tokio::signal::unix::signal(SignalKind::interrupt())
        .map_err(|e| error!("Error initializing sigint signal: {e}."))?;

    tokio::spawn(async move {
        tokio::select! {
            _ = sigint_stream.recv() => {
                info!("Received CTRL-C, shutting down..");
                shutdown.trigger();
            }
            _ = sigterm_stream.recv() => {
                info!("Received SIGTERM, shutting down..");
                shutdown.trigger();
            }
        }
    });

    let response_invoice_timeout = config.response_invoice_timeout;
    if let Some(timeout) = response_invoice_timeout {
        if timeout == 0 {
            error!("Error: response_invoice_timeout must be more than 0 seconds.");
            exit(1);
        }
    }

    let handler = Arc::new(OfferHandler::new(config.response_invoice_timeout));
    let messenger = LndkOnionMessenger::new();

    let server_fut = setup_server(
        args.lnd.clone(),
        config.grpc_host,
        config.grpc_port,
        data_dir,
        config.tls_ip,
        Arc::clone(&handler),
        address,
    )
    .await
    .map_err(|e| error!("Error setting up server: {:?}", e))?;

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
