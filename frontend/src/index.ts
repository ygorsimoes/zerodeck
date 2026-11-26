import { daemonOrigin, webSocketBase } from './origin.ts';
import { WSRouter } from './wsrouter.ts';

declare global {
  interface Window {
    zerodeckAuthToken?: string;
    DeckyBackend?: WSRouter;
    deckyAuthToken?: string;
  }
}

// The daemon serves this bundle, so its URL names the daemon's host and port.
const origin = daemonOrigin(import.meta.url);

async function fetchAuthToken(): Promise<string> {
  const res = await fetch(`${origin}/auth/token`);
  if (!res.ok) {
    throw new Error(`auth token request failed with HTTP ${res.status}`);
  }
  const token = await res.text();
  window.zerodeckAuthToken = token;
  window.deckyAuthToken = token;
  return token;
}

// `start()` retries with backoff until zerodeckd is reachable, so the UI recovers even when Steam
// loads before the daemon is listening.
const router = new WSRouter({
  baseUrl: webSocketBase(origin),
  refreshAuthToken: fetchAuthToken,
});
window.DeckyBackend = router;
router.start();
