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

async function bootstrap(): Promise<void> {
  const token = await fetchAuthToken();

  const router = new WSRouter({ refreshAuthToken: fetchAuthToken });
  window.DeckyBackend = router;
  await router.connect(token);

  console.info('[ZeroDeck:Boot] WebSocket router connected');
}

void bootstrap();
