use lightning::offers::offer::Offer;
use lightning::offers::parse::Bolt12ParseError;

// Decodes a bech32 string into an LDK offer.
pub fn decode(offer_str: String) -> Result<Offer, Bolt12ParseError> {
    offer_str.parse::<Offer>()
}
