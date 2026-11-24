# ZeroDeck Clean-Room Wire & Plugin Compatibility Protocol

This document defines the public wire, IPC, HTTP, and filesystem contracts required for full Decky Loader plugin compatibility.

## 1. HTTP & WebSocket Server (`127.0.0.1:1337`)

### Authentication & CORS
- `GET /auth/token` returns the daemon's ephemeral CSRF token (`text/plain`).
- Protected HTTP endpoints require header `X-Decky-Auth: <token>` (or `?auth=<token>` on `/ws` and `/fetch`).
- CORS origin allowlist: `https://steamloopback.host` with `allow_credentials = true`.
- `Host` allowlist (DNS-rebinding defense): every request carrying a `Host` header must name a loopback host (`localhost`, `127.0.0.0/8`, or `[::1]`, any port); anything else gets `403 Forbidden`.
- The `?auth=` token is redacted from HTTP request trace spans.
- `/ws` and `/fetch` are **not** public. `/fetch` accepts the token as `?auth=<token>` or the header; `/ws` requires `?auth=<token>` (browsers cannot set headers on a WebSocket upgrade).
- Token comparison is constant-time, and `..` path segments (including percent-encoded `%2e%2e`) are never treated as public.
- Public/allowlisted paths (`GET`/`HEAD` only, except the first two): `/auth/token`, `/healthz`, `/frontend/*`, `/static/*`, `/locales/*`, `/steam_resource/*`, `/plugins/load_main/*`, `/plugins/*/assets/*`, `/plugins/*/data/*`, `/plugins/*/dist/*`.

### WebSocket Router (`/ws?auth=<token>`)
Messages are JSON objects keyed by integer `type`:

| `type` | Name | Direction | Fields |
|---:|---|---|---|
| `-1` | `ERROR` | Server -> Client | `id`, `error: { name, error, traceback }` |
| `0` | `CALL` | Client -> Server | `id`, `route`, `args` |
| `1` | `REPLY` | Server -> Client | `id`, `result` |
| `2` | `DISCARD` | Server -> Client | `id` |
| `3` | `RECEIVED_RESPONSE` | Client -> Server | `id` |
| `4` | `FULL_SYNC` | Client -> Server | `messages: CallMessage[]` |
| `5` | `EVENT` | Server -> Client | `event`, `args` |

`FULL_SYNC` replays in-flight `CALL` messages upon WebSocket reconnection so pending calls recover seamlessly. The server dispatches each replayed call again and answers it with a normal `REPLY`/`ERROR` (there is no cross-connection reply cache yet, so replayed calls must be safe to repeat).

Connection limits:
- Inbound messages and frames above 16 MiB (`ServerConfig::ws_max_message_bytes`) close the connection.
- The server sends a WebSocket `ping` every 30 s (`ServerConfig::ws_ping_interval`); a client that has not answered the previous ping with a `pong` by the next one is disconnected. Browsers answer pings automatically.
- Clients reconnect with exponential backoff (1 s doubling up to 30 s, reset after a successful open) and fetch a fresh token from `/auth/token` before each attempt.

## 2. Plugin Backend IPC (JSON Lines over Unix Domain Socket / Inherited FD)

Each line is a UTF-8 JSON object terminated by `\n`:

| `type` | Name | Direction | Fields |
|---:|---|---|---|
| `0` | `CALL` | Daemon -> Worker | `id` (UUID string), `method`, `args`, `legacy` (optional bool) |
| `1` | `RESPONSE` | Worker -> Daemon | `id`, `success`, `res`, `error` (optional) |
| `2` | `EVENT` | Worker -> Daemon | `event`, `args` |
| — | `CONTROL` | Daemon -> Worker | `uninstall: bool` |

When `success` is `false`, `res` holds the error message string and the worker adds `error: { "name": string, "traceback": string | null }` (the exception class name and formatted traceback), which maps onto the WebSocket `ERROR` frame's `name`/`traceback`. Receivers must treat `error` as optional and ignore it if malformed.

The worker takes ownership of the inherited IPC socket FD (`--fd`) and closes it on shutdown.

## 3. Plugin Manifests & Environment

### `plugin.json`
```json
{
  "name": "Example Plugin",
  "author": "Author",
  "flags": ["debug", "root"],
  "api_version": 1
}
```
- `api_version` defaults to `0` when omitted.
- `"root"` in `flags` keeps effective `root` UID/GID; otherwise privileges drop to the host user.

### `package.json` (Optional)
- `version`: string
- `type`: `"module"` selects `ESMODULE_V1` (`load_type = 1`); otherwise `LEGACY_EVAL_IIFE` (`load_type = 0`).
- `remote_binary`: array of `{ "name": string, "url": string, "sha256hash"?: string }`.

### Environment Variables Exported to Plugin Workers
`HOME`, `USER`, `DECKY_VERSION`, `DECKY_USER`, `DECKY_USER_HOME`, `DECKY_HOME`, `DECKY_PLUGIN_SETTINGS_DIR`, `DECKY_PLUGIN_RUNTIME_DIR`, `DECKY_PLUGIN_LOG_DIR`, `DECKY_PLUGIN_DIR`, `DECKY_PLUGIN_NAME`, `DECKY_PLUGIN_VERSION`, `DECKY_PLUGIN_AUTHOR`.
