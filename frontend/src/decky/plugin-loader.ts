import {
  createLegacyPluginApi,
  disposeDeckyApi,
  type LegacyPluginApi,
  type LoaderApiOptions,
  type RouterLike,
} from './api.ts';

/** Plugin metadata returned by `loader/get_plugins`. */
export interface PluginSummary {
  name: string;
  version?: string | null;
  load_type: number;
  disabled: boolean;
  passive: boolean;
  state: string;
  failure_reason?: string | null;
}

/** Public plugin object returned by `definePlugin(() => ({ ... }))`. */
export interface PluginDefinition {
  name?: string;
  version?: string;
  content?: unknown;
  icon?: unknown;
  titleView?: unknown;
  alwaysRender?: boolean;
  onDismount?: () => void;
  [key: string]: unknown;
}

export interface LoadedPlugin {
  name: string;
  version?: string | null;
  load_type: number;
  definition: PluginDefinition;
}

export interface PluginLoadError {
  name: string;
  error: string;
}

export interface PluginLoaderDeps {
  origin: string;
  getToken: () => string | undefined;
  router: RouterLike;
  /** Injectable for tests; defaults to the browser's dynamic ESM import. */
  importModule?: (url: string) => Promise<{ default?: unknown }>;
  /** Injectable for tests; defaults to authenticated `fetch().text()`. */
  fetchText?: (url: string, headers: Record<string, string>) => Promise<string>;
  /** Called after a successful load/unload so the Quick Access Menu can re-render. */
  onChange?: () => void;
  onError?: (error: PluginLoadError) => void;
}

const ESMODULE_V1 = 1;

/**
 * Loads Decky plugin bundles without recompiling them. ESM plugins import from
 * `/plugins/<name>/dist/index.js`; legacy plugins evaluate `/plugins/<name>/frontend_bundle` and
 * receive the compatibility API object.
 */
export class PluginLoader {
  private readonly deps: Required<Pick<PluginLoaderDeps, 'origin' | 'getToken' | 'router'>> &
    PluginLoaderDeps;
  private readonly loaded = new Map<string, LoadedPlugin>();
  private readonly summaries = new Map<string, PluginSummary>();
  private readonly errors = new Map<string, string>();
  private readonly listeners = new Set<() => void>();
  private generation = 0;

  constructor(deps: PluginLoaderDeps) {
    this.deps = deps;
  }

  /** All successfully loaded plugin definitions, in backend order. */
  plugins(): LoadedPlugin[] {
    return [...this.loaded.values()];
  }

  /** Per-plugin load failures, for the Quick Access Menu and diagnostics. */
  loadErrors(): PluginLoadError[] {
    return [...this.errors.entries()].map(([name, error]) => ({ name, error }));
  }

  /** Backend summaries from the last sync, including disabled and passive plugins. */
  pluginSummaries(): PluginSummary[] {
    return [...this.summaries.values()];
  }

  /** Subscribes a UI host to load/unload/reload changes. */
  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  /**
   * Reconciles loaded bundles with the backend list: removes disabled/absent plugins, skips
   * unchanged ones, and loads new or replaced versions. A failure in one plugin never prevents the
   * others from loading.
   */
  async sync(summaries: PluginSummary[]): Promise<void> {
    this.summaries.clear();
    for (const summary of summaries) {
      this.summaries.set(summary.name, summary);
    }

    const enabled = new Set(
      summaries.filter((summary) => !summary.disabled).map((summary) => summary.name),
    );
    for (const name of [...this.loaded.keys()]) {
      if (!enabled.has(name)) {
        this.unload(name);
      }
    }

    for (const summary of summaries) {
      if (summary.disabled) continue;
      const existing = this.loaded.get(summary.name);
      if (
        existing &&
        existing.version === (summary.version ?? null) &&
        existing.load_type === summary.load_type
      ) {
        continue;
      }
      await this.loadPlugin(summary);
    }
    this.notifyChange();
  }

  /** Loads or replaces one plugin, isolating failures to that plugin. */
  async loadPlugin(summary: PluginSummary): Promise<void> {
    this.unload(summary.name);
    this.errors.delete(summary.name);
    try {
      const definition =
        summary.load_type === ESMODULE_V1
          ? await this.loadEsm(summary)
          : await this.loadLegacy(summary);
      if (!definition || typeof definition !== 'object') {
        throw new Error('plugin entry did not return an object');
      }
      this.loaded.set(summary.name, {
        name: summary.name,
        version: summary.version ?? null,
        load_type: summary.load_type,
        definition,
      });
    } catch (err: unknown) {
      const message = err instanceof Error ? `${err.name}: ${err.message}` : String(err);
      this.errors.set(summary.name, message);
      this.deps.onError?.({ name: summary.name, error: message });
    }
  }

  /** Unloads one plugin and calls its `onDismount` hook exactly once. */
  unload(name: string): void {
    const existing = this.loaded.get(name);
    if (!existing) return;
    try {
      existing.definition.onDismount?.();
    } catch (err: unknown) {
      console.warn(`[ZeroDeck] onDismount failed for ${name}`, err);
    }
    disposeDeckyApi(name);
    this.loaded.delete(name);
    this.notifyChange();
  }

  /** Reloads one plugin with a fresh generation URL. */
  async reload(name: string): Promise<void> {
    const summary = this.summaries.get(name);
    if (!summary) return;
    this.generation += 1;
    await this.loadPlugin(summary);
    this.notifyChange();
  }

  /** Unloads everything, used when Steam navigates or the loader shuts down. */
  unloadAll(): void {
    for (const name of [...this.loaded.keys()]) {
      this.unload(name);
    }
  }

  private async loadEsm(summary: PluginSummary): Promise<PluginDefinition> {
    const moduleUrl = `${this.deps.origin}/plugins/${encodeURIComponent(summary.name)}/dist/index.js?generation=${encodeURIComponent(
      this.generationKey(summary),
    )}`;
    const importModule =
      this.deps.importModule ??
      ((url: string) => import(/* @vite-ignore */ url) as Promise<{ default?: unknown }>);
    const module = await importModule(moduleUrl);
    const factory = module.default;
    if (typeof factory !== 'function') {
      throw new Error('ES module default export is not a plugin factory');
    }
    return (await factory()) as PluginDefinition;
  }

  private async loadLegacy(summary: PluginSummary): Promise<PluginDefinition> {
    const url = `${this.deps.origin}/plugins/${encodeURIComponent(summary.name)}/frontend_bundle`;
    const fetchText =
      this.deps.fetchText ??
      (async (target: string, headers: Record<string, string>) => {
        const response = await fetch(target, { headers });
        if (!response.ok) throw new Error(`frontend_bundle returned HTTP ${response.status}`);
        return response.text();
      });
    const token = this.deps.getToken() ?? '';
    const source = await fetchText(url, { 'X-Decky-Auth': token });
    // Decky evaluates the IIFE bundle as an expression and calls the returned factory with the
    // legacy API object. Wrapping it in `return (...)` gives that expression semantics without a
    // free-floating `eval`.
    const factory = new Function(
      `return (\n${source}\n);\n//# sourceURL=decky://zerodeck/legacy_plugin/${encodeURIComponent(summary.name)}/index.js`,
    )() as (api: LegacyPluginApi) => PluginDefinition;
    if (typeof factory !== 'function') {
      throw new Error('legacy bundle did not evaluate to a plugin factory');
    }
    const options: LoaderApiOptions = {
      origin: this.deps.origin,
      getToken: this.deps.getToken,
    };
    return factory(createLegacyPluginApi(this.deps.router, summary.name, options));
  }

  private generationKey(summary: PluginSummary): string {
    return `${summary.version ?? '0'}-${summary.load_type}-${this.generation}`;
  }

  private notifyChange(): void {
    this.deps.onChange?.();
    for (const listener of this.listeners) {
      listener();
    }
  }
}
