use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, HeaderMap, HeaderName, LOCATION,
    PROXY_AUTHORIZATION, TRANSFER_ENCODING, WWW_AUTHENTICATE,
};
use reqwest::{Body, Client, ClientBuilder, Method, Response, StatusCode, redirect};
use url::{Host, Url};

use crate::CoreError;

/// Default maximum number of HTTP redirect hops followed by [`send_checked_request`].
pub const DEFAULT_MAX_REDIRECTS: usize = 5;

/// Response returned after SSRF policy, DNS, and redirect validation.
pub type ValidatedResponse = Response;

/// Outbound HTTP SSRF guard shared across plugin installs and the `/fetch` CORS proxy.
///
/// Permits `http://` and `https://` targets on public and private RFC 1918 / IPv6 ULA LAN
/// addresses while blocking loopback, unspecified (`0.0.0.0/8`, `::`), link-local / cloud metadata
/// (`169.254.0.0/16`, `fe80::/10`), multicast/broadcast, and non-HTTP schemes across both initial
/// targets and redirect hops.
#[derive(Debug, Clone, Default)]
pub struct OutboundHttpPolicy {
    allow_loopback_for_tests: bool,
    allowed_loopback_ports: Vec<u16>,
    connect_timeout: Option<Duration>,
    response_timeout: Option<Duration>,
    user_agent: Option<String>,
}

/// Pooled [`reqwest::Client`] paired with the [`OutboundHttpPolicy`] enforced on its DNS resolver,
/// redirect policy, and proxy configuration.
#[derive(Debug, Clone)]
pub struct CheckedHttpClient {
    inner: Client,
    policy: OutboundHttpPolicy,
}

impl CheckedHttpClient {
    /// Builds a [`CheckedHttpClient`] using `policy`'s defaults (`redirect::Policy::none()`,
    /// `.no_proxy()`, `PolicyDnsResolver`, and any configured timeouts or user agent).
    pub fn new(policy: OutboundHttpPolicy) -> Result<Self, CoreError> {
        Self::from_builder(policy, Client::builder())
    }

    /// Builds a [`CheckedHttpClient`] from a caller-supplied [`reqwest::ClientBuilder`], layering
    /// `policy`'s mandatory redirect, no-proxy, DNS guard, and optional timeout/user-agent config.
    pub fn from_builder(
        policy: OutboundHttpPolicy,
        builder: ClientBuilder,
    ) -> Result<Self, CoreError> {
        let inner = policy
            .configure_client_builder(builder)
            .build()
            .map_err(|err| CoreError::HttpRequest(format!("failed to build HTTP client: {err}")))?;
        Ok(Self { inner, policy })
    }

    /// Underlying pooled [`reqwest::Client`].
    #[must_use]
    pub fn inner(&self) -> &Client {
        &self.inner
    }

    /// Outbound SSRF policy bound to this client.
    #[must_use]
    pub fn policy(&self) -> &OutboundHttpPolicy {
        &self.policy
    }
}

impl OutboundHttpPolicy {
    /// Strict production SSRF policy blocking all loopback, link-local, metadata, and unspecified
    /// targets.
    #[must_use]
    pub fn production() -> Self {
        Self::default()
    }

    /// Test-only helper enabling or disabling loopback access across all ports while keeping
    /// metadata, unspecified, multicast, and non-HTTP scheme blocks intact.
    #[must_use]
    pub fn with_allow_loopback_for_tests(mut self, allow: bool) -> Self {
        self.allow_loopback_for_tests = allow;
        self
    }

    /// Test-only policy permitting loopback connections exclusively to the listed local ports
    /// (for example, an ephemeral mock HTTP server bound on `127.0.0.1:0`), while still blocking
    /// all other loopback ports (such as `:1337` or `:8080`), metadata IPs, and non-HTTP schemes.
    #[must_use]
    pub fn for_loopback_ports_for_testing(ports: Vec<u16>) -> Self {
        Self {
            allow_loopback_for_tests: false,
            allowed_loopback_ports: ports,
            connect_timeout: None,
            response_timeout: None,
            user_agent: None,
        }
    }

    /// Configures optional connect and total response timeouts applied when building HTTP clients.
    #[must_use]
    pub fn with_timeouts(
        mut self,
        connect_timeout: Option<Duration>,
        response_timeout: Option<Duration>,
    ) -> Self {
        self.connect_timeout = connect_timeout;
        self.response_timeout = response_timeout;
        self
    }

    /// Configures a `User-Agent` header applied when building HTTP clients.
    #[must_use]
    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// Returns a [`reqwest::ClientBuilder`] pre-configured with no automatic redirects, no system
    /// proxy bypasses, a DNS resolver enforcing this policy, and any configured timeouts or user
    /// agent.
    pub fn client_builder(&self) -> ClientBuilder {
        self.configure_client_builder(Client::builder())
    }

    /// Applies this policy's mandatory redirect, no-proxy, and DNS safeguards (plus any configured
    /// timeouts or user agent) to an existing [`reqwest::ClientBuilder`].
    pub fn configure_client_builder(&self, builder: ClientBuilder) -> ClientBuilder {
        let mut builder = builder
            .redirect(redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(PolicyDnsResolver {
                policy: self.clone(),
            }));
        if let Some(connect) = self.connect_timeout {
            builder = builder.connect_timeout(connect);
        }
        if let Some(response) = self.response_timeout {
            builder = builder.timeout(response);
        }
        if let Some(user_agent) = &self.user_agent {
            builder = builder.user_agent(user_agent);
        }
        builder
    }

    /// Builds a [`reqwest::Client`] pre-configured with this policy's redirect, proxy, and DNS
    /// safeguards.
    pub fn build_client(&self) -> Result<Client, CoreError> {
        self.client_builder()
            .build()
            .map_err(|err| CoreError::HttpRequest(format!("failed to build HTTP client: {err}")))
    }

    /// Builds a pooled [`CheckedHttpClient`] enforcing this policy.
    pub fn build_checked_client(&self) -> Result<CheckedHttpClient, CoreError> {
        let inner = self.build_client()?;
        Ok(CheckedHttpClient {
            inner,
            policy: self.clone(),
        })
    }

    /// Validates an IP address against this policy's SSRF blocklist.
    pub fn validate_ip(&self, ip: IpAddr) -> Result<(), CoreError> {
        self.validate_ip_inner(ip, None)
    }

    /// Validates an IP address and target port against this policy, honoring test-only loopback
    /// port allowances if configured.
    pub fn validate_ip_for_port(&self, ip: IpAddr, port: u16) -> Result<(), CoreError> {
        self.validate_ip_inner(ip, Some(port))
    }

    /// Validates a raw URL string (`http://` / `https://`, non-empty host, non-blocked IP/hostname).
    pub fn validate_url(&self, raw: &str) -> Result<Url, CoreError> {
        if raw.trim().is_empty() {
            return Err(CoreError::HttpPolicy("outbound URL is empty".to_owned()));
        }
        let url = Url::parse(raw)
            .map_err(|err| CoreError::HttpPolicy(format!("invalid outbound URL `{raw}`: {err}")))?;
        self.validate_parsed_url(&url)?;
        Ok(url)
    }

    /// Resolves and validates a `Location` redirect header relative to `current`.
    pub fn validate_redirect_target(
        &self,
        current: &Url,
        location: &str,
    ) -> Result<Url, CoreError> {
        let resolved = current.join(location).map_err(|err| {
            CoreError::HttpPolicy(format!(
                "invalid redirect target `{location}` from `{current}`: {err}"
            ))
        })?;
        self.validate_parsed_url(&resolved)?;
        Ok(resolved)
    }

    fn allows_loopback_port(&self, port: Option<u16>) -> bool {
        self.allow_loopback_for_tests
            || port.is_some_and(|p| self.allowed_loopback_ports.contains(&p))
    }

    fn validate_parsed_url(&self, url: &Url) -> Result<u16, CoreError> {
        let scheme = url.scheme();
        if scheme != "http" && scheme != "https" {
            return Err(CoreError::HttpPolicy(format!(
                "disallowed URL scheme `{scheme}`"
            )));
        }

        let port = url
            .port_or_known_default()
            .filter(|&p| p != 0)
            .ok_or_else(|| {
                CoreError::HttpPolicy(format!("missing or invalid port in URL `{url}`"))
            })?;

        match url.host() {
            Some(Host::Ipv4(v4)) => self.validate_ip_for_port(IpAddr::V4(v4), port)?,
            Some(Host::Ipv6(v6)) => self.validate_ip_for_port(IpAddr::V6(v6), port)?,
            Some(Host::Domain(domain)) => {
                let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
                if normalized.is_empty() {
                    return Err(CoreError::HttpPolicy(
                        "outbound URL host is empty".to_owned(),
                    ));
                }
                if is_localhost_domain(&normalized) && !self.allows_loopback_port(Some(port)) {
                    return Err(CoreError::HttpPolicy(format!(
                        "blocked localhost target `{domain}:{port}`"
                    )));
                }
            }
            None => {
                return Err(CoreError::HttpPolicy(format!(
                    "outbound URL `{url}` has no host"
                )));
            }
        }

        Ok(port)
    }

    fn validate_ip_inner(&self, ip: IpAddr, port: Option<u16>) -> Result<(), CoreError> {
        match ip {
            IpAddr::V4(v4) => self.validate_ipv4(v4, port),
            IpAddr::V6(v6) => {
                if let Some(mapped) = v6.to_ipv4_mapped() {
                    return self.validate_ipv4(mapped, port);
                }
                let octets = v6.octets();
                if octets[..12] == [0; 12] && !v6.is_unspecified() && !v6.is_loopback() {
                    let compat = Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15]);
                    return self.validate_ipv4(compat, port);
                }
                if v6.is_loopback() {
                    if !self.allows_loopback_port(port) {
                        return Err(CoreError::HttpPolicy(format!(
                            "blocked loopback address `{v6}`"
                        )));
                    }
                    return Ok(());
                }
                if v6.is_unspecified() {
                    return Err(CoreError::HttpPolicy(format!(
                        "blocked unspecified address `{v6}`"
                    )));
                }
                if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                    return Err(CoreError::HttpPolicy(format!(
                        "blocked link-local address `{v6}`"
                    )));
                }
                if v6.is_multicast() {
                    return Err(CoreError::HttpPolicy(format!(
                        "blocked multicast address `{v6}`"
                    )));
                }
                Ok(())
            }
        }
    }

    fn validate_ipv4(&self, v4: Ipv4Addr, port: Option<u16>) -> Result<(), CoreError> {
        if v4.is_loopback() {
            if !self.allows_loopback_port(port) {
                return Err(CoreError::HttpPolicy(format!(
                    "blocked loopback address `{v4}`"
                )));
            }
            return Ok(());
        }
        if v4.octets()[0] == 0 {
            return Err(CoreError::HttpPolicy(format!(
                "blocked unspecified network address `{v4}`"
            )));
        }
        if v4.is_link_local() {
            return Err(CoreError::HttpPolicy(format!(
                "blocked link-local/metadata address `{v4}`"
            )));
        }
        if v4.is_multicast() || v4.is_broadcast() {
            return Err(CoreError::HttpPolicy(format!(
                "blocked non-unicast address `{v4}`"
            )));
        }
        Ok(())
    }

    async fn precheck_domain_for_port(&self, url: &Url) -> Result<(), CoreError> {
        if self.allow_loopback_for_tests || self.allowed_loopback_ports.is_empty() {
            return Ok(());
        }
        let port = self.validate_parsed_url(url)?;
        if let Some(Host::Domain(domain)) = url.host() {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|err| {
                    CoreError::HttpRequest(format!("DNS resolution failed for `{domain}`: {err}"))
                })?
                .collect();
            if addrs.is_empty() {
                return Err(CoreError::HttpPolicy(format!(
                    "DNS resolution returned no addresses for `{domain}`"
                )));
            }
            for addr in &addrs {
                self.validate_ip_for_port(addr.ip(), port)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BlockedByDnsPolicy(String);

impl fmt::Display for BlockedByDnsPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for BlockedByDnsPolicy {}

#[derive(Debug)]
struct PolicyDnsResolver {
    policy: OutboundHttpPolicy,
}

impl reqwest::dns::Resolve for PolicyDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = self.policy.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() {
                return Err(Box::new(BlockedByDnsPolicy(format!(
                    "DNS resolution returned no addresses for `{host}`"
                ))) as Box<dyn StdError + Send + Sync>);
            }
            for addr in &addrs {
                let allowed =
                    if !policy.allowed_loopback_ports.is_empty() && addr.ip().is_loopback() {
                        Ok(())
                    } else {
                        policy.validate_ip(addr.ip())
                    };
                if let Err(err) = allowed {
                    return Err(Box::new(BlockedByDnsPolicy(err.to_string()))
                        as Box<dyn StdError + Send + Sync>);
                }
            }
            let iter: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

/// Validates and dispatches an outbound HTTP request using a pooled [`CheckedHttpClient`],
/// enforcing its [`OutboundHttpPolicy`] across DNS resolution and up to `max_redirects` redirect
/// hops.
///
/// The wrapped [`reqwest::Client`] disables automatic redirects and system proxies and uses
/// `PolicyDnsResolver` so resolved IPs are validated at connect time while preserving connection
/// pooling, configured timeouts, and user agent. Each redirect `Location` is validated before
/// issuing the next hop, and sensitive headers (including `Host`) are stripped on cross-origin
/// redirects.
pub async fn send_checked_request(
    client: &CheckedHttpClient,
    mut method: Method,
    url: &str,
    mut headers: HeaderMap,
    mut body: Option<Body>,
    max_redirects: usize,
) -> Result<ValidatedResponse, CoreError> {
    let policy = client.policy();
    let mut current_url = policy.validate_url(url)?;
    let mut had_body = body.is_some();
    let mut reusable_body = body.as_ref().and_then(|b| b.as_bytes().map(<[u8]>::to_vec));

    for hop in 0..=max_redirects {
        policy.validate_parsed_url(&current_url)?;
        policy.precheck_domain_for_port(&current_url).await?;

        let mut req_builder = client.inner().request(method.clone(), current_url.clone());
        req_builder = req_builder.headers(headers.clone());
        if let Some(req_body) = body.take() {
            req_builder = req_builder.body(req_body);
        }

        let response = req_builder.send().await.map_err(|err| {
            if let Some(msg) = extract_dns_policy_block(&err) {
                CoreError::HttpPolicy(msg)
            } else if err.is_timeout() {
                CoreError::HttpTimeout(format!("request to `{current_url}` timed out: {err}"))
            } else {
                CoreError::HttpRequest(format!("request to `{current_url}` failed: {err}"))
            }
        })?;

        let status = response.status();
        if !is_redirect_status(status) {
            return Ok(response);
        }

        let Some(location_val) = response.headers().get(LOCATION) else {
            return Ok(response);
        };

        if hop >= max_redirects {
            return Err(CoreError::HttpPolicy(format!(
                "outbound redirect limit exceeded (max {max_redirects} hops)"
            )));
        }

        let location = location_val.to_str().map_err(|err| {
            CoreError::HttpPolicy(format!(
                "invalid Location header in redirect response: {err}"
            ))
        })?;

        let next_url = policy.validate_redirect_target(&current_url, location)?;

        if is_cross_origin(&current_url, &next_url) {
            strip_sensitive_headers(&mut headers);
        }

        match status {
            StatusCode::SEE_OTHER => {
                if method != Method::GET && method != Method::HEAD {
                    method = Method::GET;
                }
                strip_body_headers(&mut headers);
                had_body = false;
                reusable_body = None;
                body = None;
            }
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => {
                if method == Method::POST {
                    method = Method::GET;
                    strip_body_headers(&mut headers);
                    had_body = false;
                    reusable_body = None;
                    body = None;
                } else {
                    body = clone_body_for_redirect(had_body, reusable_body.as_deref())?;
                }
            }
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                body = clone_body_for_redirect(had_body, reusable_body.as_deref())?;
            }
            _ => {}
        }

        current_url = next_url;
    }

    Err(CoreError::HttpPolicy(format!(
        "outbound redirect limit exceeded (max {max_redirects} hops)"
    )))
}

fn extract_dns_policy_block(err: &(dyn StdError + 'static)) -> Option<String> {
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(blocked) = e.downcast_ref::<BlockedByDnsPolicy>() {
            return Some(blocked.0.clone());
        }
        cur = e.source();
    }
    None
}

fn is_localhost_domain(normalized: &str) -> bool {
    normalized == "localhost" || normalized.ends_with(".localhost")
}

fn is_redirect_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn is_cross_origin(a: &Url, b: &Url) -> bool {
    a.scheme() != b.scheme()
        || a.host_str() != b.host_str()
        || a.port_or_known_default() != b.port_or_known_default()
}

fn strip_sensitive_headers(headers: &mut HeaderMap) {
    headers.remove(AUTHORIZATION);
    headers.remove(COOKIE);
    headers.remove(HeaderName::from_static("cookie2"));
    headers.remove(HOST);
    headers.remove(PROXY_AUTHORIZATION);
    headers.remove(WWW_AUTHENTICATE);
}

fn strip_body_headers(headers: &mut HeaderMap) {
    headers.remove(CONTENT_LENGTH);
    headers.remove(CONTENT_TYPE);
    headers.remove(TRANSFER_ENCODING);
}

fn clone_body_for_redirect(
    had_body: bool,
    reusable_body: Option<&[u8]>,
) -> Result<Option<Body>, CoreError> {
    match (had_body, reusable_body) {
        (false, _) => Ok(None),
        (true, Some(bytes)) => Ok(Some(Body::from(bytes.to_vec()))),
        (true, None) => Err(CoreError::HttpRequest(
            "cannot replay streaming request body across HTTP redirect".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use reqwest::dns::Resolve;

    use super::*;

    #[tokio::test]
    async fn policy_dns_resolver_blocks_localhost_in_production_mode() {
        let resolver = PolicyDnsResolver {
            policy: OutboundHttpPolicy::production(),
        };
        let name: reqwest::dns::Name = "localhost".parse().expect("valid dns name");
        let err = resolver
            .resolve(name)
            .await
            .err()
            .expect("production DNS resolver must reject localhost IP");
        assert!(
            extract_dns_policy_block(err.as_ref()).is_some(),
            "expected BlockedByDnsPolicy error, got {err}"
        );
    }
}
