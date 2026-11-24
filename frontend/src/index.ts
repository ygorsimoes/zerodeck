import { WSRouter } from './wsrouter.ts';

declare global {
  interface Window {
    zerodeckAuthToken?: string;
    DeckyBackend?: WSRouter;
    deckyAuthToken?: string;
  }
}

async function fetchAuthToken(): Promise<string> {
  const res = await fetch('http://127.0.0.1:1337/auth/token');
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
const router = new WSRouter({ refreshAuthToken: fetchAuthToken });
window.DeckyBackend = router;
router.start();
