import { daemonOrigin, webSocketBase } from './origin.ts';
import { WSRouter } from './wsrouter.ts';

interface CefInjectionInfo {
  instance: string;
  context: string;
  attempt: string;
}

declare global {
  interface Window {
    zerodeckAuthToken?: string;
    DeckyBackend?: WSRouter;
    deckyAuthToken?: string;
    __zerodeckCef?: CefInjectionInfo;
    __zerodeckLoaderLoaded?: boolean;
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

/**
 * Confirms the CEF attempt that injected this bundle. The daemon binds the confirmation to its
 * instance, the CEF target and the attempt id, so a stale page cannot acknowledge a new injection.
 */
async function confirmCef(router: WSRouter): Promise<void> {
  const cef = window.__zerodeckCef;
  if (!cef) return;
  try {
    await router.call('zerodeck/cef/confirm', { ...cef, qam_ready: false });
  } catch (err: unknown) {
    console.warn('[ZeroDeck] CEF confirmation failed', err);
  }
}

// `start()` retries with backoff until zerodeckd is reachable, so the UI recovers even when Steam
// loads before the daemon is listening.
const router = new WSRouter({
  baseUrl: webSocketBase(origin),
  refreshAuthToken: fetchAuthToken,
  onOpen: () => {
    void confirmCef(router);
  },
});
window.addEventListener('zerodeck:cef-attempt', () => {
  void confirmCef(router);
});
window.DeckyBackend = router;
router.start();
