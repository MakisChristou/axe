use std::time::{Duration, Instant};

use eyre::{Result, WrapErr, eyre};
use reqwest::StatusCode;

use super::types::{
    ChainsResponse, QuoteOutcome, QuoteRequest, QuoteResponse, StatusResponse, TimedQuote,
    TokensResponse,
};
use crate::types::Network;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RfqEnvironment {
    Devnet,
    Testnet,
    Mainnet,
}

impl RfqEnvironment {
    const fn base_url(self) -> &'static str {
        match self {
            Self::Devnet => "https://devnet.api.axelar.network/rfq/v1",
            Self::Testnet => "https://testnet.api.axelar.network/rfq/v1",
            Self::Mainnet => "https://api.axelar.network/rfq/v1",
        }
    }
}

impl TryFrom<Network> for RfqEnvironment {
    type Error = eyre::Report;

    fn try_from(network: Network) -> Result<Self> {
        match network {
            Network::DevnetAmplifier => Ok(Self::Devnet),
            Network::Testnet => Ok(Self::Testnet),
            Network::Mainnet => Ok(Self::Mainnet),
            Network::Stagenet => Err(eyre!(
                "Axelar RFQ has no stagenet endpoint; use devnet-amplifier, testnet, or mainnet"
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RfqClient {
    base_url: &'static str,
    client: reqwest::Client,
}

impl RfqClient {
    pub fn for_network(network: Network) -> Result<Self> {
        let base_url = RfqEnvironment::try_from(network)?.base_url();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { base_url, client })
    }

    pub async fn chains(&self) -> Result<ChainsResponse> {
        self.get_json("chains").await
    }

    pub async fn tokens(&self) -> Result<TokensResponse> {
        self.get_json("tokens?backend=intent").await
    }

    pub async fn quote(&self, request: &QuoteRequest) -> Result<QuoteOutcome> {
        let started = Instant::now();
        let url = format!("{}/quote", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(request)
            .send()
            .await
            .wrap_err_with(|| format!("RFQ POST {url} request failed"))?;
        let response_url = response.url().to_string();
        let status = response.status();
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(QuoteOutcome::Unavailable(response_message(response).await));
        }
        if !status.is_success() {
            return Err(http_error(
                "POST",
                &response_url,
                status,
                &response_message(response).await,
            ));
        }
        let body = response
            .json::<QuoteResponse>()
            .await
            .wrap_err("RFQ quote response did not match the expected schema")?;
        let Some(quote) = body
            .quotes
            .into_iter()
            .find(|quote| quote.backend.kind == "intent")
        else {
            return Ok(QuoteOutcome::Unavailable(
                "no intent quote returned".to_owned(),
            ));
        };
        Ok(QuoteOutcome::Available(Box::new(TimedQuote {
            quote,
            latency: started.elapsed(),
        })))
    }

    pub async fn status(&self, quote_id: &str) -> Result<StatusResponse> {
        let url = format!("{}/status", self.base_url);
        let response = self
            .client
            .get(&url)
            .query(&[("quoteId", quote_id)])
            .send()
            .await
            .wrap_err_with(|| format!("RFQ GET {url} request failed"))?;
        let response_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() {
            return Err(http_error(
                "GET",
                &response_url,
                status,
                &response_message(response).await,
            ));
        }
        response
            .json::<StatusResponse>()
            .await
            .wrap_err("RFQ status response did not match the expected schema")
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}/{path}", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .wrap_err_with(|| format!("RFQ GET {url} request failed"))?;
        let response_url = response.url().to_string();
        let status = response.status();
        if !status.is_success() {
            return Err(http_error(
                "GET",
                &response_url,
                status,
                &response_message(response).await,
            ));
        }
        response
            .json::<T>()
            .await
            .wrap_err_with(|| format!("RFQ {path} response did not match the expected schema"))
    }
}

async fn response_message(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_else(|_| "response body unavailable".to_owned())
        .chars()
        .take(500)
        .collect()
}

fn http_error(method: &str, url: &str, status: StatusCode, message: &str) -> eyre::Report {
    eyre!("RFQ {method} {url} returned HTTP {status}: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_networks_to_fixed_environments() {
        assert_eq!(
            RfqEnvironment::try_from(Network::DevnetAmplifier).unwrap(),
            RfqEnvironment::Devnet
        );
        assert_eq!(
            RfqEnvironment::try_from(Network::Testnet).unwrap(),
            RfqEnvironment::Testnet
        );
        assert_eq!(
            RfqEnvironment::try_from(Network::Mainnet).unwrap(),
            RfqEnvironment::Mainnet
        );
        assert!(RfqEnvironment::try_from(Network::Stagenet).is_err());
        assert_eq!(
            RfqEnvironment::Testnet.base_url(),
            "https://testnet.api.axelar.network/rfq/v1"
        );
    }

    #[test]
    fn http_errors_include_method_and_url() {
        let error = http_error(
            "GET",
            "https://testnet.api.axelar.network/rfq/v1/chains",
            StatusCode::NOT_FOUND,
            "404 page not found",
        );

        assert_eq!(
            error.to_string(),
            "RFQ GET https://testnet.api.axelar.network/rfq/v1/chains returned HTTP 404 Not Found: 404 page not found"
        );
    }
}
