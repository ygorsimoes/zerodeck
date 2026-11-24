# ZeroDeck Clean-Room Wire & Plugin Compatibility Protocol

This document defines the public wire, IPC, HTTP, and filesystem contracts required for full Decky Loader plugin compatibility.

## 1. HTTP & WebSocket Server (`127.0.0.1:1337`)

### Authentication & CORS
- `GET /auth/token` returns the daemon's ephemeral CSRF token (`text/plain`).
- Protected HTTP endpoints require header `X-Decky-Auth: <token>` (or `?auth=<token>` on `/ws` and `/fetch`).
- CORS origin allowlist: `https://steamloopback.host` with `allow_credentials = true`.
- Public/allowlisted paths: `/auth/token`, `/healthz`, `/ws`, `/fetch`, `/frontend/*`, `/static/*`, `/locales/*`, `/steam_resource/*`, `/plugins/load_main/*`, `/plugins/*/assets/*`, `/plugins/*/data/*`, `/plugins/*/dist/*`.

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

`FULL_SYNC` replays in-flight `CALL` messages upon WebSocket reconnection so pending calls recover seamlessly or receive cached replies.

## 2. Plugin Backend IPC (JSON Lines over Unix Domain Socket / Inherited FD)

Each line is a UTF-8 JSON object terminated by `\n`:

| `type` | Name | Direction | Fields |
|---:|---|---|---|
| `0` | `CALL` | Daemon -> Worker | `id` (UUID string), `method`, `args`, `legacy` (optional bool) |
| `1` | `RESPONSE` | Worker -> Daemon | `id`, `success`, `res` |
| `2` | `EVENT` | Worker -> Daemon | `event`, `args` |
| — | `CONTROL` | Daemon -> Worker | `uninstall: bool` |

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
