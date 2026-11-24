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

### Legacy HTTP Methods (`POST /methods/{method_name}`)
- `POST /methods/ping`: returns `200 OK` `{ "success": true, "result": "pong" }`.
- `POST /methods/get_plugins`: returns `200 OK` `{ "success": true, "result": ["<plugin_name>", ...] }` in `pluginOrder` + directory order.
- `POST /methods/call_plugin_method`: accepts JSON `{ "plugin_name": string, "method_name": string, "args"?: object | array, "legacy"?: bool }`.
  - When `legacy` is omitted, infers `api_version == 0` from the target plugin's `plugin.json`; omitted `args` defaults to `{}` (legacy) or `[]` (`api_version >= 1`).
  - Rejects empty or `_`-prefixed private methods (`400 Bad Request`), unknown plugins (`404 Not Found`), and disabled, passive, failed, or stopped workers (`503 Service Unavailable`).
  - Returns `200 OK` `{ "success": true, "result": <value> }` on success, or `200 OK` `{ "success": false, "result": { "name": string, "error": string, "traceback": string | null } }` when the Python plugin raises an exception.

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

Implemented WS routes:
- `utilities/ping` -> `REPLY` `"pong"`.
- `loader/get_plugins` -> `REPLY` array of `{ "name", "version", "load_type", "disabled", "passive", "state" }` ordered by `pluginOrder`.
- `loader/call_plugin_method` with `args: [plugin_name, method_name, ...positional_args]` -> dispatches `legacy: false`. Private `_`-prefixed methods are rejected with `PermissionError`.
- `loader/call_legacy_plugin_method` with `args: [plugin_name, method_name, kwargs_object?]` -> dispatches `legacy: true` (defaults omitted kwargs to `{}`).
- `loader/reload_plugin` with `args: [plugin_name]` -> restarts the plugin worker cleanly and replies `true`.
- `loader/plugin_event` (`EVENT = 5` server -> client): `args` is a 1-element array `[{ "plugin": string, "event": string, "args": Value[] }]` matching `WSRouter` listener spreading (`listener(...data.args)`).

If a plugin worker crashes or stops while a call is in flight (or is in `BackingOff`/`StartupTimeout`), the server responds with `DISCARD` (`2`) for that call ID.

`FULL_SYNC` replays in-flight `CALL` messages upon WebSocket reconnection so pending calls recover seamlessly. Each inbound `CALL` and replayed call runs in its own spawned task so slow plugin calls never starve WebSocket keepalive `pong` handling. (There is no cross-connection reply cache yet, so replayed calls must be safe to repeat.)

Connection limits:
- Inbound messages and frames above 16 MiB (`ServerConfig::ws_max_message_bytes`) close the connection.
- The server sends a WebSocket `ping` every 30 s (`ServerConfig::ws_ping_interval`); a client that has not answered the previous ping with a `pong` by the next one is disconnected. Browsers answer pings automatically.
- Clients reconnect with exponential backoff (1 s doubling up to 30 s, reset after a successful open) and fetch a fresh token from `/auth/token` before each attempt.

## 2. Plugin Backend IPC (JSON Lines over Unix Domain Socket / Inherited FD)

Each line is a UTF-8 JSON object terminated by `\n` (up to 4 MiB per frame):

| `type` | Name | Direction | Fields |
|---:|---|---|---|
| `0` | `CALL` | Daemon -> Worker | `id` (UUID string), `method`, `args`, `legacy` (optional bool) |
| `1` | `RESPONSE` | Worker -> Daemon | `id`, `success`, `res`, `error` (optional) |
| `2` | `EVENT` | Worker -> Daemon | `event`, `args` |
| — | `CONTROL` | Daemon -> Worker | `uninstall: bool` |

When `success` is `false`, `res` holds the error message string and the worker adds `error: { "name": string, "traceback": string | null }` (the exception class name and formatted traceback), which maps onto the WebSocket `ERROR` frame's `name`/`traceback`. Receivers must treat `error` as optional and ignore it if malformed.

The daemon spawns each active plugin over an anonymous Unix `socketpair()` FD (`--fd`, clearing `FD_CLOEXEC` in `pre_exec`) with a sanitized environment (`env_clear()`), `cwd` set to `DECKY_PLUGIN_DIR`, and `stdout`/`stderr` piped into structured `tracing` logs. The worker takes ownership of the inherited IPC socket FD and closes it on shutdown. On graceful uninstall (`Control { uninstall: true }` followed by socket write-half shutdown), the worker completes `_unload` and `_uninstall` before exiting; hung workers are terminated with `SIGKILL` after the shutdown grace timeout.

## 3. Plugin Manifests, Settings & Python Runtime

### `plugin.json`
```json
{
  "name": "Example Plugin",
  "author": "Author",
  "flags": ["debug", "root"],
  "api_version": 1
}
```
- `api_version` defaults to `0` when omitted. Legacy `api_version == 0` plugins support `@classmethod`, `@staticmethod`, and plain `Plugin` methods via keyword arguments.
- `"root"` in `flags` keeps effective `root` UID/GID; otherwise privileges drop (`setgroups`, `setresgid`, `setresuid` when running as `root`) to the resolved non-root host user.

### `package.json` (Optional)
- `version`: string
- `type`: `"module"` selects `ESMODULE_V1` (`load_type = 1`); otherwise `LEGACY_EVAL_IIFE` (`load_type = 0`).
- `remote_binary`: array of `{ "name": string, "url": string, "sha256hash"?: string }`.

### `settings/loader.json`
- `pluginOrder`: optional array of `plugin.json` `name` strings; listed plugins appear first in that order, with remaining plugins sorted deterministically by directory name.
- `disabledPlugins` (also accepts `disabled_plugins` on read): optional array of `plugin.json` `name` strings whose workers are not spawned and whose RPC calls return disabled errors.

### Python Worker Modules & Environment
- `zerodeck-py-host` injects `decky` (and `decky_plugin` alias), `settings` (`SettingsManager` with `read()`, `commit()`, `getSetting()`, `setSetting()`, atomic writes, and corrupt-file preservation), and `helpers` (`get_ssl_context()`, `get_homebrew_path()`, `get_user()`).
- `<plugin_dir>/py_modules` is prepended to `sys.path` (`sys.path[0]`) when present, and synchronous plugin methods run on worker threads (`asyncio.to_thread`) so blocking I/O does not stall `emit()` or concurrent RPCs.
- Environment variables exported to plugin workers: `HOME`, `USER`, `DECKY_VERSION`, `DECKY_USER`, `DECKY_USER_HOME`, `DECKY_HOME`, `DECKY_PLUGIN_SETTINGS_DIR`, `DECKY_PLUGIN_RUNTIME_DIR`, `DECKY_PLUGIN_LOG_DIR`, `DECKY_PLUGIN_DIR`, `DECKY_PLUGIN_NAME`, `DECKY_PLUGIN_VERSION`, `DECKY_PLUGIN_AUTHOR`.
