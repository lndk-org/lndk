# LNDK

LNDK is a standalone daemon that connects to [LND](https://github.com/lightningnetwork/lnd) (via its grpc API) that aims to implement [bolt 12](https://github.com/lightning/bolts/pull/798) functionality _externally_ to LND. LNDK leverages the [lightning development kit](https://github.com/lightningdevkit/rust-lightning) to provide functionality, acting as a thin "shim" between LND's APIs and LDK's lightning library.

Project Milestones:
- [x] [v0.1.0](https://github.com/lndk-org/lndk/milestone/1): Onion message forwarding for LND.
- [ ] [v0.2.0](https://github.com/lndk-org/lndk/milestone/2): Payment to offers with blinded paths.

*Please note that this project is still experimental.*

## Resources
* [Contributor guidelines](https://github.com/lndk-org/lndk/blob/master/CONTRIBUTING.md)
* [Code of Conduct](https://github.com/lndk-org/lndk/blob/master/code_of_conduct.md)
* [Architecture](https://github.com/lndk-org/lndk/blob/master/ARCH.md)

When you encounter a problem with `LNDK`, Feel free to file issues or start [a discussion](https://github.com/lndk-org/lndk/discussions). If your question doesn't fit in either place, find us in the [BOLT 12 Discord](https://discord.gg/Pk7mT3FQFn) in the lndk channel.

## Setting up LNDK

#### Compiling LND

To run `LNDK`, you require access to a `LND` node running _at least_ [LND v0.17.0](https://github.com/lightningnetwork/lnd/releases/tag/v0.17.0-beta).

You will need to compile `LND` in `dev` mode (to enable protocol-level feature handling externally) and enable the `peersrpc`, `signerrpc`, and `walletrpc` sub-servers:

`make install tags="peersrpc signrpc walletrpc dev"`

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

#### Two options for LNDK

Now that we have LND set up properly, there are two key things you can do with LNDK: 
1) Forward onion messages. By increasing the number of Lightning nodes out there that can forward onion messages, this increases the anonymity set and helps to bootstrap BOLT 12 for more private payments.
2) Pay BOLT 12 offers, a more private standard for receiving payments over Lightning, which also allows for static invoices.

To accomplish #1, follow the instructions below to get the LNDK binary up and running. For #2, you can [use the CLI](https://github.com/lndk-org/lndk/blob/master/docs/cli_commands.md).

#### Running LNDK

Now we need to set up LNDK. To start:

```
git clone https://github.com/lndk-org/lndk
cd lndk
```
In order for `LNDK` successfully connect to `LND`, we need to pass in the grpc address and authentication credentials. There are two ways to do this:

1) These values can be passed in via the command line when running the `LNDK` program, like this:

`cargo run --bin=lndk -- --address=<ADDRESS> --cert=<TLSPATH> --macaroon=<MACAROONPATH>`

Or in a more concrete example:

`cargo run --bin=lndk -- --address=https://localhost:10009 --cert=/home/<USERNAME>/.lnd/tls.cert --macaroon=/home/<USERNAME>/.lnd/data/chain/bitcoin/regtest/admin.macaroon`

**Remember** that the grpc address must start with https:// for the program to work.

2) Alternatively, you can use a configuration file to add the required arguments.

* In the lndk directory, create file named `lndk.conf`.
* Add the following lines to the file:
  * `address="<ADDRESS"`
  * `cert="<TLSPATH>"`
  * `macaroon="<MACAROONPATH>"`
* Run `cargo run --bin=lndk -- --conf lndk.conf`

- Use any of the commands with the --help option for more information about each argument.

#### Custom macaroon

Rather than use the admin.macaroon with unrestricted permission to an `LND` node, we can bake a macaroon using lncli with much more specific permissions for better security. With this command, generate a macaroon which will give `LNDK` only the specific grpc endpoints it's designed to hit:

```
lncli bakemacaroon --save_to=<FILEPATH>/lndk.macaroon uri:/lnrpc.Lightning/GetInfo uri:/lnrpc.Lightning/ListPeers uri:/lnrpc.Lightning/SubscribePeerEvents uri:/lnrpc.Lightning/SendCustomMessage uri:/lnrpc.Lightning/SubscribeCustomMessages uri:/peersrpc.Peers/UpdateNodeAnnouncement uri:/signrpc.Signer/DeriveSharedKey
```

## Security

NOTE: It is recommended to always use [cargo-crev](https://github.com/crev-dev/cargo-crev)
to verify the trustworthiness of each of your dependencies, including this one.
