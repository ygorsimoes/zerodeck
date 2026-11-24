from __future__ import annotations

import os
import ssl
from pathlib import Path

_CA_CANDIDATES = (
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
)


def get_ssl_context() -> ssl.SSLContext:
    """Returns a default TLS SSLContext using environment or standard system CA bundles."""
    cafile = os.getenv("SSL_CERT_FILE") or None
    capath = os.getenv("SSL_CERT_DIR") or None
    if cafile is None and capath is None:
        for candidate in _CA_CANDIDATES:
            if Path(candidate).is_file():
                cafile = candidate
                break
    return ssl.create_default_context(cafile=cafile, capath=capath)


def get_homebrew_path() -> str:
    """Returns the resolved `DECKY_HOME` directory."""
    return os.getenv("DECKY_HOME", "")


def get_user() -> str:
    """Returns the unprivileged host user name (`DECKY_USER`)."""
    return os.getenv("DECKY_USER") or os.getenv("USER", "")
