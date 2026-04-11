use lightning::{
    offers::offer::{Amount, Offer},
    onion_message::messenger::Destination,
};
use std::str::FromStr;

use super::OfferError;

/// Decodes a bech32 offer string into an LDK offer.
pub fn decode(offer_str: String) -> Result<Offer, OfferError> {
    Offer::from_str(&offer_str).map_err(OfferError::ParseOfferFailure)
}

/// Get the destination of an offer.
pub async fn get_destination(offer: &Offer) -> Result<Destination, OfferError> {
    if offer.paths().is_empty() {
        if let Some(signing_pubkey) = offer.issuer_signing_pubkey() {
            Ok(Destination::Node(signing_pubkey))
        } else {
            Err(OfferError::IntroductionNodeNotFound)
        }
    } else {
        Ok(Destination::BlindedPath(offer.paths()[0].clone()))
    }
}
/// Checks that the user-provided amount matches the provided offer or invoice.
///
/// Parameters:
///
/// * `offer_amount_msats`: The amount set in the offer or invoice.
/// * `amount_msats`: The amount we want to pay.
pub async fn validate_amount(
    offer_amount_msats: Option<&Amount>,
    pay_amount_msats: Option<u64>,
) -> Result<u64, OfferError> {
    let validated_amount = match offer_amount_msats {
        Some(offer_amount) => {
            match *offer_amount {
                Amount::Bitcoin {
                    amount_msats: bitcoin_amt,
                } => {
                    // Per BOLT 12 (bolts #1316): readers MUST NOT respond to
                    // offers where offer_amount is zero. A zero amount when
                    // present is semantically invalid since omitting the field
                    // already indicates no minimum is required.
                    if bitcoin_amt == 0 {
                        return Err(OfferError::InvalidAmount(
                            "Offer has amount explicitly set to 0, which is invalid per BOLT 12 spec"
                                .to_string(),
                        ));
                    }
                    if let Some(msats) = pay_amount_msats {
                        if msats < bitcoin_amt {
                            return Err(OfferError::InvalidAmount(format!(
                                "{msats} is less than offer amount {bitcoin_amt}"
                            )));
                        }
                        msats
                    } else {
                        bitcoin_amt
                    }
                }
                _ => {
                    return Err(OfferError::InvalidCurrency);
                }
            }
        }
        None => {
            if let Some(msats) = pay_amount_msats {
                msats
            } else {
                return Err(OfferError::InvalidAmount(
                    "Offer doesn't set an amount, so user must specify one".to_string(),
                ));
            }
        }
    };
    Ok(validated_amount)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use bitcoin::{
        key::{Keypair, Secp256k1},
        secp256k1::{PublicKey, SecretKey},
    };
    use lightning::offers::offer::{OfferBuilder, Quantity};

    use super::*;

    fn build_custom_offer(amount_msats: u64) -> Offer {
        let secp_ctx = Secp256k1::new();
        let keys = Keypair::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap());
        let pubkey = PublicKey::from(keys);

        let expiration = SystemTime::now() + Duration::from_secs(24 * 60 * 60);
        OfferBuilder::new(pubkey)
            .description("coffee".to_string())
            .amount_msats(amount_msats)
            .supported_quantity(Quantity::Unbounded)
            .absolute_expiry(expiration.duration_since(SystemTime::UNIX_EPOCH).unwrap())
            .issuer("Foo Bar".to_string())
            .build()
            .unwrap()
    }
    #[tokio::test]
    async fn test_validate_amount() {
        // If the amount the user provided is greater than the offer-provided amount, then
        // we should be good.
        let offer = build_custom_offer(20000);
        let offer_amount = offer.amount();
        assert!(validate_amount(offer_amount.as_ref(), Some(20000))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn test_validate_amount_is_invalid() {
        // If the amount the user provided is lower than the offer amount, we error.
        let offer = build_custom_offer(20000);
        let offer_amount = offer.amount();
        assert!(validate_amount(offer_amount.as_ref(), Some(1000))
            .await
            .is_err());
    }

    /// Per BOLT 12 spec clarification (https://github.com/lightning/bolts/pull/1316):
    /// - Readers MUST NOT respond to offers where offer_amount is zero
    /// - Writers MUST set offer_amount greater than zero when present
    ///
    /// An offer with amount=0 is semantically invalid because omitting
    /// offer_amount already indicates no minimum is required. A zero value
    /// when present is incorrect per the spec.
    #[tokio::test]
    async fn test_reject_offer_with_zero_amount() {
        // With the rust-lightning fix (PR #4487), OfferBuilder::amount_msats(0)
        // is silently normalized to None (no amount) at build time. Verify
        // that building an offer with amount=0 produces amount=None.
        let offer = build_custom_offer(0);
        assert!(
            offer.amount().is_none(),
            "OfferBuilder must normalize amount_msats(0) to None per BOLT 12"
        );

        // Directly test that validate_amount rejects Amount::Bitcoin { amount_msats: 0 }.
        // This covers the case where a raw TLV-decoded offer somehow has amount=0
        // (e.g. from a non-compliant implementation). The rust-lightning parser
        // already rejects these, but we defend in depth.
        let zero_amount = Amount::Bitcoin { amount_msats: 0 };
        assert!(
            validate_amount(Some(&zero_amount), Some(20000))
                .await
                .is_err(),
            "Offers with amount=0 must be rejected per BOLT 12 spec (bolts #1316)"
        );
        assert!(
            validate_amount(Some(&zero_amount), None).await.is_err(),
            "Offers with amount=0 must be rejected per BOLT 12 spec (bolts #1316)"
        );
    }
}
