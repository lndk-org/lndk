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
use lndk::lnd::LndCfg;
use lndk::{Cfg, LifecycleSignals, LndkOnionMessenger, OfferHandler};
use log::LevelFilter;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{Receiver, Sender};

#[macro_use]
extern crate configure_me;

#[tokio::main]
async fn main() -> Result<(), ()> {
    let config = Config::including_optional_config_files(&["./lndk.conf"])
        .unwrap_or_exit()
        .0;

    let lnd_args = LndCfg::new(config.address, config.cert, config.macaroon);
    let log_level = match config.log_level {
        Some(level_str) => match LevelFilter::from_str(&level_str) {
            Ok(level) => level,
            Err(_) => {
                // Since the logger isn't set up yet, we use a println just this once.
                println!(
                    "User provided log level '{}' is invalid. Make sure it is set to either 'error',
                    'warn', 'info', 'debug' or 'trace'",
                    level_str
                );
                return Err(());
            }
        },
        None => LevelFilter::Info,
    };
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
        log_dir: config.log_dir,
        log_level,
        signals,
    };

    let handler = OfferHandler::new();
    let messenger = LndkOnionMessenger::new(handler);
    messenger.run(args).await
}
