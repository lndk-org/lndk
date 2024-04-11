mod internal {
    #![allow(unused_imports)]
    #![allow(clippy::enum_variant_names)]
    #![allow(clippy::unnecessary_lazy_evaluations)]
    #![allow(clippy::useless_conversion)]
    #![allow(clippy::never_loop)]
    #![allow(clippy::uninlined_format_args)]

    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

use internal::*;
use lndk::lnd::{validate_lnd_creds, LndCfg};
use lndk::{setup_logger, Cfg, LifecycleSignals, LndkOnionMessenger, OfferHandler};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

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
    let lnd_args = LndCfg::new(config.address, creds);

    let (shutdown, listener) = triggered::trigger();
    // Create the channel which will tell us when the onion messenger has finished starting up.
    let (tx, _): (Sender<u32>, Receiver<u32>) = mpsc::channel(1);
    let signals = LifecycleSignals {
        shutdown,
        listener,
        started: tx,
    };
    let args = Cfg {
        lnd: lnd_args,
        signals,
    };

    let handler = Arc::new(OfferHandler::new());
    setup_logger(config.log_level, config.log_dir)?;

    let messenger = LndkOnionMessenger::new();
    messenger.run(args, Arc::clone(&handler)).await
}
