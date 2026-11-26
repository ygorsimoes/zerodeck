/** Daemon address used when the bundle was not loaded from the daemon (tests, `file:` URLs). */
export const FALLBACK_ORIGIN = 'http://127.0.0.1:1337';

/**
 * Returns the origin of the daemon that served this bundle, so a non-default `--port` keeps
 * working; any URL that is not HTTP(S) falls back to {@link FALLBACK_ORIGIN}.
 */
export function daemonOrigin(moduleUrl: string | undefined): string {
  if (moduleUrl === undefined || !URL.canParse(moduleUrl)) {
    return FALLBACK_ORIGIN;
  }
  const url = new URL(moduleUrl);
  return url.protocol === 'http:' || url.protocol === 'https:' ? url.origin : FALLBACK_ORIGIN;
}

/** Turns an HTTP(S) origin into the matching WebSocket base URL (`http` → `ws`, `https` → `wss`). */
export function webSocketBase(origin: string): string {
  return origin.replace(/^http/, 'ws');
}
