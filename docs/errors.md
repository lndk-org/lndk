# LNDK Error Handling

LNDK returns structured error information through gRPC status codes and error details to help clients handle different failure scenarios.

## Error Structure

LNDK errors have three components:
1. **gRPC Status Code**: Standard codes like `INVALID_ARGUMENT`, `INTERNAL`, `UNAVAILABLE`
2. **Error Code**: LNDK-specific identifier (e.g., `PARSE_OFFER_FAILURE`, `INVALID_AMOUNT`)
3. **Human Message**: Descriptive error message

## Error Categories

LNDK returns three main error types:
- **OfferError**: Offer parsing, validation, and payment processing errors
- **LndError**: LND connectivity and configuration errors  
- **AuthError**: Authentication and authorization errors

## Client Requirements

To properly see LNDK's structured error details in your client, you need gRPC error details support. Your client must support gRPC's rich error details mechanism (`google.rpc.ErrorInfo`) and use a gRPC library that supports status details extraction to get the LNDK-specific error code.

You'll need to import `google/rpc/error_details.proto` (located at [`proto/external/google/rpc/error_details.proto`](../proto/external/google/rpc/error_details.proto) in our project, [original source](https://github.com/googleapis/googleapis/blob/master/google/rpc/error_details.proto)) in your client.

## Client example: grpcurl

`grpcurl` is a command-line tool for testing gRPC services. Here's how to use it with LNDK:

Test with an invalid offer to see error details:

```bash
grpcurl -insecure \
  -H "macaroon: $(xxd -ps -u -c 1000 '~/.lnd/data/chain/bitcoin/regtest/admin.macaroon')" \
  -import-path proto \
  -proto proto/lndkrpc.proto \
  -d '{"offer": "", "amount": 10000}' \
  127.0.0.1:7000 lndkrpc.Offers/GetInvoice
```

Response:
```
ERROR:
  Code: InvalidArgument
  Message: The provided offer was invalid. Please provide a valid offer in bech32 format, i.e. starting with 'lno'. Error: Bech32(Parse(Char(MissingSeparator)))
  Details:
  1)    {
          "@type": "type.googleapis.com/google.rpc.ErrorInfo",
          "domain": "lndk",
          "reason": "PARSE_OFFER_FAILURE"
        }
```