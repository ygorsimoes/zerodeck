import { useEffect, useState } from 'react';
import type { PluginLoader } from '../decky/plugin-loader.ts';
import { setQuickAccessVisible } from '../decky/visibility.ts';
import type { WSRouter } from '../wsrouter.ts';
import { type QamPluginEntry, QamView } from './view.ts';

export interface QamAppProps {
  router: WSRouter;
  loader: PluginLoader;
  origin: string;
  getToken: () => string | undefined;
}

interface PluginSummaryLike {
  name: string;
  version?: string | null;
  load_type: number;
  disabled: boolean;
  passive: boolean;
  state: string;
}

async function fetchCefState(origin: string, token: string | undefined): Promise<string> {
  try {
    const headers: Record<string, string> = token ? { 'X-Decky-Auth': token } : {};
    const response = await fetch(`${origin}/cef/status`, { headers });
    if (!response.ok) return `unavailable (HTTP ${response.status})`;
    const status = (await response.json()) as { state?: string };
    return status.state ?? 'unknown';
  } catch (err: unknown) {
    return `unavailable (${err instanceof Error ? err.message : String(err)})`;
  }
}

export function QamApp(props: QamAppProps): ReturnType<typeof QamView> {
  const [open, setOpen] = useState(false);
  const [summaries, setSummaries] = useState<PluginSummaryLike[]>([]);
  const [selected, setSelected] = useState<string | undefined>();
  const [cefState, setCefState] = useState('unknown');
  const [, setRevision] = useState(0);

  useEffect(
    () => props.loader.subscribe(() => setRevision((revision) => revision + 1)),
    [props.loader],
  );

  useEffect(() => {
    const onToggle = () => setOpen((current) => !current);
    window.addEventListener('zerodeck:qam-toggle', onToggle);
    return () => window.removeEventListener('zerodeck:qam-toggle', onToggle);
  }, []);

  useEffect(() => {
    setQuickAccessVisible(open);
  }, [open]);

  useEffect(() => {
    let cancelled = false;
    const refresh = async () => {
      try {
        const list = await props.router.call<[], PluginSummaryLike[]>('loader/get_plugins');
        if (!cancelled) setSummaries(list);
      } catch (err: unknown) {
        console.warn('[ZeroDeck] QAM could not refresh plugins', err);
      }
    };
    void refresh();
    return () => {
      cancelled = true;
    };
  }, [props.router]);

  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    const poll = async () => {
      const state = await fetchCefState(props.origin, props.getToken());
      if (!cancelled) setCefState(state);
    };
    void poll();
    const timer = setInterval(() => {
      void poll();
    }, 5000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [open, props.origin, props.getToken]);

  const loaded = props.loader.plugins();
  const errors = props.loader.loadErrors();
  const plugins: QamPluginEntry[] = summaries.map((summary) => {
    const definition = loaded.find((plugin) => plugin.name === summary.name)?.definition;
    return {
      name: summary.name,
      version: summary.version ?? definition?.version ?? null,
      state: summary.state,
      disabled: summary.disabled,
      passive: summary.passive,
      content: definition?.content as QamPluginEntry['content'],
      error: errors.find((error) => error.name === summary.name)?.error,
    };
  });

  const togglePlugin = async (plugin: QamPluginEntry) => {
    try {
      await props.router.call(
        plugin.disabled ? 'loader/enable_plugin' : 'loader/disable_plugin',
        plugin.name,
      );
      const list = await props.router.call<[], PluginSummaryLike[]>('loader/get_plugins');
      setSummaries(list);
    } catch (err: unknown) {
      console.error(`[ZeroDeck] failed to toggle ${plugin.name}`, err);
    }
  };

  const reload = async (name: string) => {
    try {
      await props.router.call('loader/reload_plugin', name);
      await props.loader.reload(name);
      const list = await props.router.call<[], PluginSummaryLike[]>('loader/get_plugins');
      setSummaries(list);
    } catch (err: unknown) {
      console.error(`[ZeroDeck] failed to reload ${name}`, err);
    }
  };

  return QamView({
    open,
    plugins,
    selected,
    errors,
    cefState,
    onToggleOpen: () => setOpen((current) => !current),
    onSelect: setSelected,
    onTogglePlugin: (plugin) => {
      void togglePlugin(plugin);
    },
    onReload: (name) => {
      void reload(name);
    },
  });
}
