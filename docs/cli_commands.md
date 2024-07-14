# Interacting with LNDK server

There are two ways you can connect to the server:
- You can use `lndk-cli` to connect to a running instance of `LNDK` and pay an offer.
- Write a gRPC client in the language of your choice.

## lndk-cli installation

To use the `lndk-cli` to pay a BOLT 12 offer, follow these instructions:
- To install lndk-cli: 
	`cargo install --bin=lndk-cli --path .`
- With the above command, `lndk-cli` will be installed to `~/.cargo/bin`. So make sure `~/.cargo/bin` is on your PATH so we can properly run `lndk-cli`.
- Run `lndk-cli -h` to make sure it's working. You'll see output similar to:

```
A cli for interacting with lndk

Usage: lndk-cli [OPTIONS] <COMMAND>

Commands:
  decode-offer    Decodes a bech32-encoded offer string into a BOLT 12 offer
  decode-invoice  Decodes a bech32-encoded invoice string into a BOLT 12 invoice
  pay-offer       PayOffer pays a BOLT 12 offer, provided as a 'lno'-prefaced offer string
  help            Print this message or the help of the given subcommand(s)

Options:
  -n, --network <NETWORK>              Global variables [default: regtest]          
  -m, --macaroon-path <MACAROON_PATH>           
      --macaroon-hex <MACAROON_HEX>    
  -h, --help                           Print help
```

### lndk-cli commands

Once `lndk-cli` is installed, you can use it to pay an offer.

Since `lndk-cli` needs to connect to lnd, you'll need to provide an lnd macaroon to the binary. 

If your macaroon is not in the default lnd location, you'll need to specify them manually.

If your macaroon is in the default location, paying an offer looks like:

`lndk-cli pay-offer <OFFER_STRING> <AMOUNT_MSATS>`

If your macaroon is not in the default location, an example command looks like:

`lndk-cli -- --network=mainnet --macaroon-path=/credentials/custom.macaroon pay-offer <OFFER_STRING> <AMOUNT_MSATS>`

Or you can pass in the credentials directly with a macaroon string like:
`lndk-cli -- --network=mainnet --macaroon-hex=<MACAROON_HEX_STR> pay-offer <OFFER_STRING> <AMOUNT_MSATS>`

## gRPC client example

Another option for interacting with `LNDK` is to connect to the LNDK server with a gRPC client,
which you can do in [most languages](https://grpc.io/docs/languages/).

Again, since LNDK needs to connect to LND, you'll need to pass in your LND macaroon to establish a connection. Note that:
- The client must pass in this data via gRPC metadata. You can find an example of this in the [Rust client](https://github.com/lndk-org/lndk/blob/master/src/cli.rs) used to connect `lndk-cli` to the server.

## TLS: Running `lndk-cli` remotely

When `LNDK` is started up, self-signed TLS credentials are automatically generated and stored in `~/.lndk`. If you're running `lndk-cli` locally, it'll know where to find the certificate file it needs to establish a secure connection with the LNDK server.

To run `lndk-cli` on a remote machine, users need to copy the `tls-cert.pem` file to the corresponding LNDK data directory (`~/.lndk`) on the machine where `lndk-cli` is being run.
