## Eclair testing instructions

These instructions assume you already have bitcoind and lnd nodes set up on regtest.

The network we'll build will look like this:

`LND --> Eclair --> Eclair2`

Eclair2 will create an offer and we'll make sure lnd can pay the offer with the help of lndk. 

For Eclair to create an offer to pay, we need to install the tipjar plugin. Pull down this branch (https://github.com/ACINQ/eclair-plugins/pull/6/files) and build `eclair-plugins`:

`mvn package -DskipTests`

The resulting jar will be located at `eclair-plugins/bolt12-tip-jar/target/bolt12-tip-jar-0.9.1-SNAPSHOT.jar`

Set up two eclair directories, for example at `~/eclair1` and `~/eclair2` with the following configuration within (at `eclair.conf`):

```
eclair {
  chain=regtest

  server {
    port=9995
    public-ips = [ "https://localhost" ]
  }

  bitcoind {
    rpcuser="PASSWORD"
    rpcpassword="PASSWORD"
    rpcport=18443
    zmqblock="tcp://127.0.0.1:28334"
    zmqtx="tcp://127.0.0.1:29335"
  }

  api {
    enabled = true
    port = 8181
    password = "testing2"
  }

  features {
    option_route_blinding = optional
  }

  tip-jar {
    description = "donation to eclair"
    default-amount-msat = 100000000 // Amount to use if the invoice request does not specify an amount
    max-final-expiry-delta = 1000 // How long (in blocks) the route to pay the invoice will be valid
  }
}
```

Just:
- Set the correct bitcoind values.
- Make sure that `server.port` and `api.port` are different for each Eclair node. 

Install the dependencies for Eclair and install Eclair with `mvn package -DskipTests`. After unzipping the eclair bin (located in `target`), run two eclair nodes, each pointing to the appropriate datadirs we set above, and load in the tipjar plugin:

`./eclair-node.sh /eclair-plugins/bolt12-tip-jar/target/bolt12-tip-jar-0.9.1-SNAPSHOT.jar -Declair.datadir="~/eclair1`

`./eclair-node.sh ../../../../../eclair-plugins/bolt12-tip-jar/target/bolt12-tip-jar-0.9.1-SNAPSHOT.jar -Declair.datadir="~/eclair2`

Using [Eclair's API](https://acinq.github.io/eclair), open two channels: 1) from LND to Eclair1, 2) from Eclair1 to Eclair2. Mine several blocks using bitcoind and make sure those channels are confirmed.

Have Eclair2 create an offer with:

`eclair-cli tipjarshowoffer`

Then pay the offer. Run `lndk-cli pay-offer` using the instructions above. 
