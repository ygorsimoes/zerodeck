use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName, LOCATION,
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
    allowed_loopback_ports: Vec<u16>,
    connect_timeout: Option<Duration>,
    response_timeout: Option<Duration>,
}

impl OutboundHttpPolicy {
    /// Strict production SSRF policy blocking all loopback, link-local, metadata, and unspecified
    /// targets.
    #[must_use]
    pub fn production() -> Self {
        Self::default()
    }

    /// Test-only policy permitting loopback connections exclusively to the listed local ports
    /// (for example, an ephemeral mock HTTP server bound on `127.0.0.1:0`), while still blocking
    /// all other loopback ports (such as `:1337` or `:8080`), metadata IPs, and non-HTTP schemes.
    #[must_use]
    pub fn for_loopback_ports_for_testing(ports: Vec<u16>) -> Self {
        Self {
            allowed_loopback_ports: ports,
            connect_timeout: None,
            response_timeout: None,
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

    /// Returns a [`reqwest::ClientBuilder`] pre-configured with no automatic redirects, no system
    /// proxy bypasses, and a DNS resolver enforcing this policy.
    pub fn client_builder(&self) -> ClientBuilder {
        let mut builder = Client::builder()
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
        builder
    }

    /// Builds a [`reqwest::Client`] pre-configured with this policy's redirect, proxy, and DNS
    /// safeguards.
    pub fn build_client(&self) -> Result<Client, CoreError> {
        self.client_builder()
            .build()
            .map_err(|err| CoreError::HttpRequest(format!("failed to build HTTP client: {err}")))
    }

    /// Validates an IP address against the production SSRF blocklist (always rejects loopback).
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
        port.is_some_and(|p| self.allowed_loopback_ports.contains(&p))
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

    async fn resolve_and_pin_hop(
        &self,
        url: &Url,
    ) -> Result<Option<(String, Vec<SocketAddr>)>, CoreError> {
        let port = self.validate_parsed_url(url)?;
        match url.host() {
            Some(Host::Ipv4(v4)) => {
                self.validate_ip_for_port(IpAddr::V4(v4), port)?;
                Ok(None)
            }
            Some(Host::Ipv6(v6)) => {
                self.validate_ip_for_port(IpAddr::V6(v6), port)?;
                Ok(None)
            }
            Some(Host::Domain(domain)) => {
                let addrs: Vec<SocketAddr> = tokio::net::lookup_host((domain, port))
                    .await
                    .map_err(|err| {
                        CoreError::HttpRequest(format!(
                            "DNS resolution failed for `{domain}`: {err}"
                        ))
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
                Ok(Some((domain.to_owned(), addrs)))
            }
            None => Err(CoreError::HttpPolicy(format!(
                "outbound URL `{url}` has no host"
            ))),
        }
    }
}

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
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    format!("DNS resolution returned no addresses for `{host}`"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            for addr in &addrs {
                let allowed =
                    if !policy.allowed_loopback_ports.is_empty() && addr.ip().is_loopback() {
                        Ok(())
                    } else {
                        policy.validate_ip(addr.ip())
                    };
                if let Err(err) = allowed {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        err.to_string(),
                    ))
                        as Box<dyn std::error::Error + Send + Sync>);
                }
            }
            let iter: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(iter)
        })
    }
}

/// Validates and dispatches an outbound HTTP request while enforcing `policy` across DNS
/// resolution and up to `max_redirects` HTTP redirect hops.
///
/// Each hop resolves DNS ahead of connecting, validates every resolved IP against `policy`, pins
/// the validated addresses on a `reqwest::ClientBuilder` configured with
/// [`redirect::Policy::none()`] and `.no_proxy()`, and re-validates every `Location` target before
/// issuing the next hop.
pub async fn send_checked_request(
    client: &Client,
    policy: &OutboundHttpPolicy,
    mut method: Method,
    url: &str,
    mut headers: HeaderMap,
    mut body: Option<Body>,
    max_redirects: usize,
) -> Result<ValidatedResponse, CoreError> {
    let _ = client;
    let mut current_url = policy.validate_url(url)?;
    let mut had_body = body.is_some();
    let mut reusable_body = body.as_ref().and_then(|b| b.as_bytes().map(<[u8]>::to_vec));

    for hop in 0..=max_redirects {
        let pinned = policy.resolve_and_pin_hop(&current_url).await?;
        let mut builder = policy.client_builder();
        if let Some((domain, addrs)) = &pinned {
            builder = builder.resolve_to_addrs(domain, addrs);
        }
        let hop_client = builder.build().map_err(|err| {
            CoreError::HttpRequest(format!("failed to build hop HTTP client: {err}"))
        })?;

        let mut req_builder = hop_client.request(method.clone(), current_url.clone());
        req_builder = req_builder.headers(headers.clone());
        if let Some(req_body) = body.take() {
            req_builder = req_builder.body(req_body);
        }

        let response = req_builder.send().await.map_err(|err| {
            CoreError::HttpRequest(format!("request to `{current_url}` failed: {err}"))
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
