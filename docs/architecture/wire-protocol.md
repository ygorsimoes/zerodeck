# ZeroDeck Clean-Room Wire & Plugin Compatibility Protocol

This document defines the public wire, IPC, HTTP, and filesystem contracts required for full Decky Loader plugin compatibility.

## 1. HTTP & WebSocket Server (`127.0.0.1:1337`)

### Authentication & CORS
- `GET /auth/token` returns the daemon's ephemeral CSRF token (`text/plain`).
- Protected HTTP endpoints require header `X-Decky-Auth: <token>` (or `?auth=<token>` on `/ws` and `/fetch`).
- CORS origin allowlist: `https://steamloopback.host` with `allow_credentials = true`.
- `Host` allowlist (DNS-rebinding defense): every request carrying a `Host` header must name a loopback host (`localhost`, `127.0.0.0/8`, or `[::1]`, any port); anything else gets `403 Forbidden`.
- Bind policy: the daemon refuses to start when `SERVER_HOST`/`--host` resolves to any non-loopback address (e.g. `0.0.0.0`, `::`, a LAN IP), because `/auth/token` would become reachable from that network and the `Host` check can be spoofed by a remote client. `--allow-non-loopback` / `ZERODECK_ALLOW_NON_LOOPBACK=1` opts in (a warning is logged); the runtime container image sets it and relies on a loopback-only port mapping (`127.0.0.1:1337:1337`).
- The `?auth=` token is redacted from HTTP request trace spans.
- `/ws` and `/fetch` are **not** public. `/fetch` accepts the token as `?auth=<token>` or `X-Decky-Auth`; `/ws` requires `?auth=<token>` (browsers cannot set headers on a WebSocket upgrade).
- Token comparison is constant-time, and `..` path segments (including percent-encoded `%2e%2e`), backslashes (`\` / `%5c`), and encoded null bytes (`%00`) are rejected with `403 Forbidden` and never treated as public.
- Public/allowlisted paths (`GET`/`HEAD` only, except `/auth/token` and `/healthz`): `/auth/token`, `/healthz`, `/frontend/*`, `/static/*`, `/locales/*`, `/steam_resource/*` (CSRF-exempt; handler reserved for Phase 4), `/plugins/load_main/*`, `/plugins/{name}/frontend_bundle`, `/plugins/{name}/assets/*`, `/plugins/{name}/data/*`, `/plugins/{name}/dist/*`.

### Static, Plugin Asset & Frontend Bundle Routes
All file routes percent-decode path segments, reject empty/`..`/backslash/null-byte segments, canonicalize against per-root `SafePath` jails, and stream bytes with extension-based `Content-Type` (`js`/`mjs` -> `text/javascript; charset=utf-8`, `css`, `json`, `html`, `svg`, `png`, `jpg`/`jpeg`, `webp`, `woff2`, `wasm`, fallback `application/octet-stream`):
- `GET /frontend/{*path}`: serves from resolved `frontend_dir` (`ZERODECK_FRONTEND_DIR` -> `/opt/zerodeck/frontend` -> `<exe_dir>/../frontend` -> `frontend/dist` -> `<CARGO_MANIFEST_DIR>/../../frontend/dist`).
- `GET /static/{*path}`: serves from `<frontend_dir>/static`.
- `GET /locales/{*path}`: serves from `<frontend_dir>/locales`.
- `GET /plugins/load_main/{name}` and `GET /plugins/{name}/frontend_bundle`: resolve `{name}` (`manifest.name` first, falling back to directory name) and serve `<plugins_dir>/<dir>/dist/index.js` with `Content-Type: text/javascript; charset=utf-8` and `Cache-Control: no-store`.
- `GET /plugins/{name}/dist/{*path}`: serves from `<plugins_dir>/<dir>/dist`.
- `GET /plugins/{name}/assets/{*path}`: serves from `<plugins_dir>/<dir>/assets`.
- `GET /plugins/{name}/data/{*path}`: serves from `<data_dir>/<dir>`.

### `/fetch` Streaming CORS Proxy
- `ANY /fetch?fetch_url=<url>` (authenticated via `?auth=<token>` or `X-Decky-Auth`):
  - Requires non-empty `fetch_url` (`400 Bad Request` if missing/invalid or non-`http`/`https` scheme) and caps inbound request bodies at `16 MiB` (`413 Payload Too Large`).
  - **SSRF & Redirect Policy:** Allows `http`/`https` only. Rejects loopback (`127.0.0.0/8`, `::1`, `localhost`, `*.localhost`), unspecified (`0.0.0.0/8`, `::`), multicast, and link-local/metadata (`169.254.0.0/16`, `fe80::/10`, including IPv4-mapped IPv6 forms) across initial URL validation, DNS resolution, and up to `5` manual HTTP redirect hops (`403 Forbidden`), stripping `Authorization`, `Cookie`, `Cookie2`, `Host`, `Proxy-Authorization`, and `WWW-Authenticate` across cross-origin redirects. Default upstream timeouts: `10 s` connect (`fetch_connect_timeout`), `60 s` response (`fetch_response_timeout`). Permits private RFC1918 / ULA LAN ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`) for local-network plugins.
  - **Header Translation:** Strips `host`, `x-decky-auth`, `x-decky-fetch-excluded-headers`, hop-by-hop headers, `content-length`, `accept-encoding`, `cookie`, and any comma-separated header names in `X-Decky-Fetch-Excluded-Headers`. Unwraps `X-Decky-Header-<Target>` into `<Target>` on the upstream request (blocking `host`, `x-decky-auth`, `content-length`, `accept-encoding`, and all hop-by-hop headers: `connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailers`, `transfer-encoding`, `upgrade`).
  - **Response Streaming:** Streams upstream status, non-hop-by-hop response headers (stripping upstream `Access-Control-*` headers so daemon CORS policy stays authoritative), and body bytes. Upstream timeouts return `504 Gateway Timeout`; other transport errors return `502 Bad Gateway`.

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
- `utilities/filepicker` with positional `[path?, include_files?, filter?, extensions?, show_hidden?, max_entries?, include_folders?]` or 1-element options object -> lists entries within `SafePathSet` (`$DECKY_USER_HOME`/`$HOME`, `/run/media`, `$UNPRIVILEGED_PATH`/`$DECKY_HOME`), returning `{ path, realpath, files: [{ name, path, realpath, isdir, size, modified }], truncated }`. Symlink targets escaping allowed roots are omitted; invalid root paths fail with `PathEscapeError`.
- `loader/get_plugins` -> `REPLY` array of `{ "name", "version", "load_type", "disabled", "passive", "state" }` ordered by `pluginOrder`.
- `loader/call_plugin_method` with `args: [plugin_name, method_name, ...positional_args]` -> dispatches `legacy: false`. Private `_`-prefixed methods are rejected with `PermissionError`.
- `loader/call_legacy_plugin_method` with `args: [plugin_name, method_name, kwargs_object?]` -> dispatches `legacy: true` (defaults omitted kwargs to `{}`).
- `loader/reload_plugin` with `args: [plugin_name]` -> restarts the plugin worker cleanly and replies `true`.
- `loader/install_plugin` & `utilities/install_plugin` with positional `[url_or_path, name?, version?, sha256?, install_type?, user_confirmed?, directory?]` or 1-element spec object `{ url_or_path (aliases: url, artifact, path), name?, version?, sha256 (alias: hash)?, install_type?, user_confirmed?, directory (alias: directory_hint)? }` -> stages and verifies ZIP archives (up to `128 MiB` by default; `http(s)://` subject to SSRF policy + mandatory SHA-256 unless `developer.allow_unverified_installs` is `true`; `file://`/local paths via `SafePathSet` requiring SHA-256 or `user_confirmed: true`), downloads `package.json` `remote_binary` assets into `<plugin_dir>/bin/<name>` (`0755`, up to `256 MiB` each by default), swaps atomically, starts the worker if enabled and non-passive, emits `loader/plugins_changed`, and replies `{ name, directory, version, sha256, replaced_existing }`.
- `loader/uninstall_plugin` with `args: [plugin_name, purge_data?]` (or `{ plugin_name, purge_data? }`) -> runs `_unload` + `_uninstall`, deletes `plugins/<dir>`, preserves `settings/` and `data/` unless `purge_data: true`, strips `pluginOrder` / `disabledPlugins`, emits `loader/plugins_changed`, and replies `true`.
- `loader/enable_plugin` / `loader/disable_plugin` with `args: [plugin_name]` (or `{ plugin_name }`) -> atomically updates `disabledPlugins`, starts or stops the plugin worker, emits `loader/plugins_changed`, and replies `true`.
- `loader/set_plugin_order` with `args: [plugin_order: string[]]` (or `{ plugin_order | pluginOrder | order }`) -> atomically updates `pluginOrder`, emits `loader/plugins_changed`, and replies `true`.
- `loader/get_settings` with `args: []` or `[dotted_key?]` (or `{ key? }`) -> returns the full `loader.json` object or dotted-path value (`null` if absent).
- `loader/set_setting` with `args: [dotted_key, value]` (or `{ key, value }`) -> atomically writes a dotted key path in `loader.json` (rejecting direct writes to `pluginOrder`, `disabledPlugins`, and `disabled_plugins` with `ValueError`) and replies `true`.

Server-to-client `EVENT` (`type: 5`) frames:
- `loader/plugin_event`: `args` is `[{ "plugin": string, "event": string, "args": Value[] }]` emitted by a worker's `decky.emit(...)`, matching `WSRouter` listener spreading (`listener(...data.args)`).
- `loader/plugins_changed`: `args` is `[PluginSummary[]]` broadcast after plugin install, uninstall, enable, disable, order changes, or debug manifest updates.
- `zerodeck/plugin_reloaded`: `args` is `[{ "plugin": string, "directory": string }]` broadcast when the debug watcher reloads a `"debug"` plugin backend.
- `zerodeck/frontend_reloaded`: `args` is `[{ "plugin": string, "directory": string }]` broadcast when the debug watcher detects changes under a `"debug"` plugin's `dist/` directory.

If a plugin worker crashes or stops while a call is in flight (or is in `BackingOff`/`StartupTimeout`), the server responds with `DISCARD` (`2`) for that call ID.

`FULL_SYNC` and `RECEIVED_RESPONSE` semantics (`WsSyncCache`, default capacity `256`, TTL `300 s`):
- Inbound `CALL (0)` and `FULL_SYNC (4)` replays are keyed by `(session, call.id, call.route, canonical_args_json)`.
- `session` comes from the optional `session` query parameter of the `/ws` upgrade (the frontend sends one `crypto.randomUUID()` per page load, stable across reconnects). It must be 1-64 characters of `[A-Za-z0-9-]`; any other value is rejected with `400 Bad Request` (after the auth check). Connections that omit it share one legacy namespace. This keeps a new page load, whose call ids restart at 1, from receiving another session's cached reply.
- If an identical call is already in flight, the replay joins the active execution and receives its single result without spawning duplicate work.
- If an identical call has completed and has not yet been acknowledged, the cached `REPLY`/`ERROR`/`DISCARD` frame is returned immediately without re-executing the call.
- `RECEIVED_RESPONSE (3)` evicts completed cached entries matching `id` within the connection's session only.
- Each inbound `CALL` and replayed call runs in its own spawned task so slow plugin calls never starve WebSocket keepalive `pong` handling.

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
- `disabledPlugins` (also accepts `disabled_plugins` on read, migrating on write): optional array of `plugin.json` `name` strings whose workers are not spawned and whose RPC calls return disabled errors.
- `developer.allow_unverified_installs`: optional boolean (default `false`); when `false`, remote archive installs and `remote_binary` downloads fail closed without a valid SHA-256 digest.
- Managed lifecycle keys (`pluginOrder`, `disabledPlugins`, `disabled_plugins`) must be mutated via `loader/set_plugin_order`, `loader/enable_plugin`, and `loader/disable_plugin` rather than `loader/set_setting`.

### Python Worker Modules & Environment
- `zerodeck-py-host` injects `decky` (and `decky_plugin` alias), `settings` (`SettingsManager` with `read()`, `commit()`, `getSetting()`, `setSetting()`, atomic writes, and corrupt-file preservation), and `helpers` (`get_ssl_context()`, `get_homebrew_path()`, `get_user()`).
- `<plugin_dir>/py_modules` is prepended to `sys.path` (`sys.path[0]`) when present, and synchronous plugin methods run on worker threads (`asyncio.to_thread`) so blocking I/O does not stall `emit()` or concurrent RPCs.
- Environment variables exported to plugin workers: `HOME`, `USER`, `DECKY_VERSION`, `DECKY_USER`, `DECKY_USER_HOME`, `DECKY_HOME`, `DECKY_PLUGIN_SETTINGS_DIR`, `DECKY_PLUGIN_RUNTIME_DIR`, `DECKY_PLUGIN_LOG_DIR`, `DECKY_PLUGIN_DIR`, `DECKY_PLUGIN_NAME`, `DECKY_PLUGIN_VERSION`, `DECKY_PLUGIN_AUTHOR`.
