# LNDK

An experimental attempt at using [LDK](https://github.com/lightningdevkit/rust-lightning) to implement [bolt 12](https://github.com/lightning/bolts/pull/798) features for [LND](https://github.com/lightningnetwork/lnd). 
* [Contributor guidelines](https://github.com/lndk-org/lndk/blob/master/CONTRIBUTING.md)
* [Code of Conduct](https://github.com/lndk-org/lndk/blob/master/code_of_conduct.md)
* [Architecture](https://github.com/lndk-org/lndk/blob/master/ARCH.md)

## Setting up LNDK

#### Compiling LND

To run `LNDK`, `LND` is assumed to be running. You'll need to make some adjustments when compiling and running `LND` to make it compatible with `LNDK`.

First, you'll need to run a particular [branch](https://github.com/lightningnetwork/lnd/tree/v0.16.2-patch-customfeatures) of `LND` to allow the advertising of onion_message feature bits. To do so, follow these steps:

```
git clone https://github.com/lightningnetwork/lnd
cd lnd
git checkout v0.16.2-patch-customfeatures
```

While on this branch, compile `LND`. Make sure that the peersrpc and signerrpc services and dev tag are enabled, like this:

`make install --tags="peersrpc signerrpc dev"`

Note that this guide assumes some familiarity with setting up `LND`. If you're looking to get up to speed, try [this guide](https://docs.lightning.engineering/lightning-network-tools/lnd/run-lnd).

#### Running LND

Once you're ready to run `LND`, the binary must be run with `--protocol.custom-message=513` to allow it to report onion messages to `LNDK` as well as `--protocol.custom-nodeann=39` `--protocol.custom-init=39` for advertising the onion message feature bits.

There are two ways you can do this:

1) Pass these options directly to `LND` when running it:

`lnd --protocol.custom-message=513 --protocol.custom-nodeann=39 --protocol.custom-init=39`

2) Adding these to the config file `lnd.conf`:

```
[protocol]
protocol.custom-message=513
protocol.custom-nodeann=39
protocol.custom-init=39
```

#### Running LNDK

In order for `LNDK` successfully connect to `LND`, we need to pass in the grpc address and authentication credentials. These values can be passed in via the command line when running the `LNDK` program, like this:

- `cargo run -- --address=<ADDRESS> --cert=<TLSPATH> --macaroon=<MACAROONPATH>`


In a more concrete example:

`cargo run -- --address=https://localhost:10009 --cert=/home/<USERNAME>/.lnd/tls.cert --macaroon=/home/<USERNAME>/.lnd/data/chain/bitcoin/regtest/admin.macaroon`

**Remember** that the grpc address must start with https:// for the program to work.

- Alternatively, you can use a configuration file to add the required arguments.

* In the lndk directory, create fle named `lndk.conf`.
* Add the following lines to the file 
  * `address="https://localhost:10009"` 
  * `cert="/home/<USERNAME>/.lnd/tls.cert"`
  * `macaroon="/home/<USERNAME>/.lnd/data/chain/bitcoin/regtest/admin.macaroon"`
* Run `cargo run -- --conf lndk.conf`

- Use any of the commands with the --help option for more information about each argument.

## Security

NOTE: It is recommended to always use [cargo-crev](https://github.com/crev-dev/cargo-crev)
to verify the trustworthiness of each of your dependencies, including this one.
