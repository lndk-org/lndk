use lightning::{
    offers::{
        offer::{Amount, Offer},
        parse::Bolt12ParseError,
    },
    onion_message::messenger::Destination,
};

use super::OfferError;

/// Decodes a bech32 offer string into an LDK offer.
pub fn decode(offer_str: String) -> Result<Offer, Bolt12ParseError> {
    offer_str.parse::<Offer>()
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
                    if let Some(msats) = pay_amount_msats {
                        if msats < bitcoin_amt {
                            return Err(OfferError::InvalidAmount(format!(
                                "{msats} is less than offer amount {}",
                                bitcoin_amt
                            )));
                        }
                        msats
                    } else {
                        // If user didn't set amount, set it to the offer amount.
                        if bitcoin_amt == 0 {
                            return Err(OfferError::InvalidAmount(
                                "Offer doesn't set an amount, so user must specify one".to_string(),
                            ));
                        }
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

        let offer = build_custom_offer(0);
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

        // Both user amount and offer amount can't be 0.
        let offer = build_custom_offer(0);
        let offer_amount = offer.amount();
        assert!(validate_amount(offer_amount.as_ref(), None).await.is_err());
    }
}
