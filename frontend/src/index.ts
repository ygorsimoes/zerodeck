import { createLoaderApi, installDeckyApi } from './decky/api.ts';
import { DFL } from './decky/dfl.ts';
import { PluginLoader, type PluginSummary } from './decky/plugin-loader.ts';
import { daemonOrigin, webSocketBase } from './origin.ts';
import { WSRouter } from './wsrouter.ts';

declare global {
  interface Window {
    zerodeckAuthToken?: string;
    DeckyBackend?: WSRouter;
    DeckyPluginLoader?: PluginLoader;
    deckyAuthToken?: string;
    DFL?: Record<string, unknown>;
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

// `@decky/ui` is externalized to the `DFL` global by the official plugin toolchain.
window.DFL = DFL;

// Install the `@decky/api` connection hook before any plugin module is imported.
installDeckyApi((version, pluginName) =>
  createLoaderApi(
    router,
    pluginName,
    { origin, getToken: () => window.zerodeckAuthToken },
    version,
  ),
);

const loader = new PluginLoader({
  origin,
  router,
  getToken: () => window.zerodeckAuthToken,
  onError: ({ name, error }) => {
    console.error(`[ZeroDeck] failed to load ${name}: ${error}`);
  },
});
window.DeckyPluginLoader = loader;

router.addEventListener('loader/plugins_changed', (plugins) => {
  void loader.sync(plugins as PluginSummary[]);
});
router.addEventListener('zerodeck/frontend_reloaded', (payload) => {
  const { plugin } = payload as { plugin?: string };
  if (plugin) void loader.reload(plugin);
});

async function loadPlugins(): Promise<void> {
  try {
    const plugins = await router.call<[], PluginSummary[]>('loader/get_plugins');
    await loader.sync(plugins);
  } catch (err: unknown) {
    console.warn('[ZeroDeck] failed to enumerate plugins', err);
  }
}

router.start();
void loadPlugins();
