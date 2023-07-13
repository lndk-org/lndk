mod internal {
    #![allow(clippy::enum_variant_names)]
    #![allow(clippy::unnecessary_lazy_evaluations)]
    #![allow(clippy::useless_conversion)]
    #![allow(clippy::never_loop)]
    #![allow(clippy::uninlined_format_args)]

    include!(concat!(env!("OUT_DIR"), "/configure_me_config.rs"));
}

use lndk::lnd::LndCfg;
use internal::*;

#[macro_use]
extern crate configure_me;

#[tokio::main]
async fn main() -> Result<(), ()> {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let lnd_config = Config::including_optional_config_files(&["./lndk.conf"])
        .unwrap_or_exit()
        .0;
    let args = LndCfg::new(lnd_config.address, lnd_config.cert, lnd_config.macaroon);

    lndk::run(args).await
}
