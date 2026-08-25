//! HTTP client using reqwest for both native and WASM targets.

use std::collections::HashMap;
use std::time::Duration;

use super::{HttpClient, HttpError, HttpResponse, REQUEST_TIMEOUT};
use crate::proxy::ProxyConfig;

/// HTTP client implementation backed by reqwest.
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    /// Create a new `ReqwestHttpClient` with an optional user agent.
    ///
    /// Native targets layer HTTP/2 and TCP keepalives on top of reqwest's
    /// defaults so a long-lived shared client survives intermediaries that
    /// reap idle HTTP/2 flows. On WASM the browser owns connection management
    /// and these knobs aren't exposed.
    ///
    /// The `user_agent` is applied on native only. In the browser a
    /// script-set `User-Agent` is honored by Firefox/Safari but dropped by
    /// Chrome; because it is not CORS-safelisted, setting it turns otherwise
    /// simple requests into preflighted ones, and some third-party endpoints
    /// (e.g. the USDT0 deployments API) reject the preflight. Browsers send
    /// their own `User-Agent` regardless, so omitting it loses nothing. This
    /// is a deliberate divergence from the upstream `spark-sdk` client, which
    /// only talks to servers it controls and so can keep the override.
    ///
    /// Fails when reqwest cannot assemble a client: an invalid `user_agent`
    /// (one that is not a valid header value), or a TLS backend or system
    /// resolver that will not initialise.
    pub fn new(user_agent: Option<String>) -> Result<Self, HttpError> {
        Self::with_proxy(user_agent, None)
    }

    /// Like [`Self::new`], but routes every request through `proxy`.
    ///
    /// Setting a proxy also switches reqwest off system-proxy autodetection,
    /// so no environment variable can redirect traffic around it. A proxy that
    /// can't be reached fails the request: there is no direct fallback.
    ///
    /// A proxy is rejected on WASM: `fetch` exposes no proxy control, so
    /// honouring it is impossible and ignoring it would connect direct behind
    /// the caller's back.
    // `user_agent` is unused on WASM by design (see above); the signature is
    // kept uniform across targets and to match the upstream client.
    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        expect(clippy::needless_pass_by_value, unused_variables)
    )]
    pub fn with_proxy(
        user_agent: Option<String>,
        proxy: Option<&ProxyConfig>,
    ) -> Result<Self, HttpError> {
        let builder = reqwest::Client::builder();

        #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
        let builder = {
            let mut builder = builder;
            if let Some(ua) = user_agent {
                builder = builder.user_agent(ua);
            }
            if let Some(proxy) = proxy {
                builder = builder.proxy(reqwest::Proxy::all(proxy.reqwest_url())?);
            }
            builder
                .tcp_keepalive(Some(Duration::from_mins(1)))
                .http2_keep_alive_interval(Duration::from_secs(30))
                .http2_keep_alive_timeout(Duration::from_secs(10))
                .http2_keep_alive_while_idle(true)
        };

        #[cfg(all(target_family = "wasm", target_os = "unknown"))]
        if proxy.is_some() {
            return Err(HttpError::Builder(
                "a SOCKS5 proxy cannot be honoured on WASM: fetch exposes no proxy control"
                    .to_string(),
            ));
        }

        Ok(Self {
            client: builder.build()?,
        })
    }
}

#[macros::async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn get(
        &self,
        url: String,
        headers: Option<HashMap<String, String>>,
    ) -> Result<HttpResponse, HttpError> {
        tracing::debug!("Making GET request to: {url}");
        let mut req = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT));

        if let Some(headers) = headers {
            for (key, value) in &headers {
                req = req.header(key, value);
            }
        }

        let response = req.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        tracing::debug!("Received response, status: {status}");
        tracing::trace!("raw response body: {body}");

        Ok(HttpResponse { status, body })
    }

    async fn post(
        &self,
        url: String,
        headers: Option<HashMap<String, String>>,
        body: Option<String>,
    ) -> Result<HttpResponse, HttpError> {
        tracing::debug!("Making POST request to: {url}");
        let mut req = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT));

        if let Some(headers) = headers {
            for (key, value) in &headers {
                req = req.header(key, value);
            }
        }
        if let Some(body) = body {
            req = req.body(body);
        }

        let response = req.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        tracing::debug!("Received response, status: {status}");
        tracing::trace!("raw response body: {body}");

        Ok(HttpResponse { status, body })
    }

    async fn delete(
        &self,
        url: String,
        headers: Option<HashMap<String, String>>,
        body: Option<String>,
    ) -> Result<HttpResponse, HttpError> {
        tracing::debug!("Making DELETE request to: {url}");
        let mut req = self
            .client
            .delete(&url)
            .timeout(Duration::from_secs(REQUEST_TIMEOUT));

        if let Some(headers) = headers {
            for (key, value) in &headers {
                req = req.header(key, value);
            }
        }
        if let Some(body) = body {
            req = req.body(body);
        }

        let response = req.send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        tracing::debug!("Received response, status: {status}");
        tracing::trace!("raw response body: {body}");

        Ok(HttpResponse { status, body })
    }
}
