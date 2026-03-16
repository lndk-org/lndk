use crate::OfferError;
use bitcoin_payment_instructions::hrn_resolution::{HrnResolution, HrnResolver, HumanReadableName};
use bitcoin_payment_instructions::http_resolver::HTTPHrnResolver;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const DEFAULT_DNS_TIMEOUT_SECS: u64 = 20;

#[derive(Clone)]
pub struct LndkDNSResolverMessageHandler {
    resolver: Arc<dyn HrnResolver + Send + Sync>,
}

impl Default for LndkDNSResolverMessageHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl LndkDNSResolverMessageHandler {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_DNS_TIMEOUT_SECS))
            .build()
            .expect("Failed to build HTTP client for DNS resolution");

        Self::with_resolver(HTTPHrnResolver::with_client(client))
    }

    pub fn with_resolver<R: HrnResolver + Send + Sync + 'static>(resolver: R) -> Self {
        Self {
            resolver: Arc::new(resolver),
        }
    }

    pub async fn resolver_hrn_to_offer(&self, name_str: &str) -> Result<String, OfferError> {
        let resolved_uri = self.resolve_locally(name_str.to_string()).await?;
        self.extract_offer_from_uri(&resolved_uri)
    }

    pub fn extract_offer_from_uri(&self, uri: &str) -> Result<String, OfferError> {
        let url = Url::parse(uri)
            .map_err(|_| OfferError::ResolveUriError("Invalid URI format".to_string()))?;

        for (key, value) in url.query_pairs() {
            if key.eq_ignore_ascii_case("lno") {
                return Ok(value.into_owned());
            }
        }

        Err(OfferError::ResolveUriError(
            "URI does not contain 'lno' parameter with BOLT12 offer".to_string(),
        ))
    }

    pub async fn resolve_locally(&self, name: String) -> Result<String, OfferError> {
        let hrn_parsed = HumanReadableName::from_encoded(&name)
            .map_err(|_| OfferError::ParseHrnFailure(name.clone()))?;

        let resolution = self
            .resolver
            .resolve_hrn(&hrn_parsed)
            .await
            .map_err(|e| OfferError::HrnResolutionFailure(format!("{}: {}", name, e)))?;

        let uri = match resolution {
            HrnResolution::DNSSEC { result, .. } => result,
            HrnResolution::LNURLPay { .. } => {
                return Err(OfferError::ResolveUriError(
                    "LNURL resolution not supported in this flow".to_string(),
                ))
            }
        };

        Ok(uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_offer_from_simple_uri() {
        let handler = LndkDNSResolverMessageHandler::new();
        let uri = "bitcoin:?lno=lno1qgsqvgnwgcg35z";
        let result = handler.extract_offer_from_uri(uri);
        assert_eq!(result.unwrap(), "lno1qgsqvgnwgcg35z");
    }

    #[test]
    fn test_extract_offer_with_percent_encoding() {
        let handler = LndkDNSResolverMessageHandler::new();
        let uri = "bitcoin:?lno=lno1%20test%3Dvalue";
        let result = handler.extract_offer_from_uri(uri);
        assert_eq!(result.unwrap(), "lno1 test=value");
    }

    #[test]
    fn test_extract_offer_missing_param() {
        let handler = LndkDNSResolverMessageHandler::new();
        let uri = "bitcoin:?amount=50&label=test";
        let result = handler.extract_offer_from_uri(uri);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not contain 'lno' parameter"));
    }
}
