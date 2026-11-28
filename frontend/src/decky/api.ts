import type { ComponentType, ReactNode } from 'react';
import { useQuickAccessVisible } from './visibility.ts';

/** Minimal router surface the loader API needs; tests inject a fake. */
export interface RouterLike {
  call<Args extends unknown[] = [], Return = unknown>(
    route: string,
    ...args: Args
  ): Promise<Return>;
  addEventListener(event: string, listener: (...args: unknown[]) => unknown): unknown;
  removeEventListener(event: string, listener: (...args: unknown[]) => unknown): void;
}

export interface LoaderApiOptions {
  /** Daemon origin used for the fetch proxy and external resource URLs. */
  origin: string;
  /** Returns the current ephemeral auth token, if the daemon handed one out. */
  getToken: () => string | undefined;
}

export interface ToastData {
  title?: ReactNode;
  body?: ReactNode;
  subtext?: ReactNode;
  icon?: ReactNode;
  logo?: ReactNode;
  duration?: number;
  [key: string]: unknown;
}

export interface ToastNotification {
  data: ToastData;
  dismiss: () => void;
}

export interface RouterHook {
  addRoute(path: string, component: ComponentType<never>): void;
  addPatch<T>(path: string, patch: T): T;
  addGlobalComponent(name: string, component: ComponentType<never>): void;
  removeRoute(path: string): void;
  removePatch<T>(path: string, patch: T): void;
  removeGlobalComponent(name: string): void;
  waitForUnlock(): Promise<void>;
}

export interface DeckyApi {
  _version: number;
  call<Args extends unknown[] = [], Return = void>(route: string, ...args: Args): Promise<Return>;
  callable<Args extends unknown[] = [], Return = void>(
    route: string,
  ): (...args: Args) => Promise<Return>;
  addEventListener(event: string, listener: (...args: unknown[]) => unknown): unknown;
  removeEventListener(event: string, listener: (...args: unknown[]) => unknown): void;
  routerHook: RouterHook;
  toaster: { toast(toast: ToastData): ToastNotification };
  openFilePicker(...args: unknown[]): Promise<never>;
  executeInTab(
    tab: string,
    runAsync: boolean,
    code: string,
  ): Promise<{ success: boolean; result: unknown }>;
  injectCssIntoTab(tab: string, style: string): string;
  removeCssFromTab(tab: string, cssId: string): void;
  fetchNoCors(
    input: string,
    init?: RequestInit & { excludedHeaders?: string[] },
  ): Promise<Response>;
  getExternalResourceURL(url: string): string;
  useQuickAccessVisible: typeof useQuickAccessVisible;
}

/** Raised for a loader API that exists in Decky but is not implemented by the clean-room runtime. */
export class UnsupportedFeatureError extends Error {
  readonly feature: string;

  constructor(feature: string) {
    super(`[ZeroDeck] ${feature} is not available in this loader build`);
    this.name = 'UnsupportedFeatureError';
    this.feature = feature;
  }
}

interface ToastStore {
  toasts: ToastNotification[];
  listeners: Set<(toasts: ToastNotification[]) => void>;
}

const toastStores = new Map<string, ToastStore>();
const cssIds = new Map<string, number>();

function toastStore(pluginName: string): ToastStore {
  let store = toastStores.get(pluginName);
  if (!store) {
    store = { toasts: [], listeners: new Set() };
    toastStores.set(pluginName, store);
  }
  return store;
}

/** Snapshot used by the Quick Access Menu toast host. */
export function getToasts(pluginName: string): ToastNotification[] {
  return [...toastStore(pluginName).toasts];
}

/** Subscribes the Quick Access Menu toast host to one plugin's toasts. */
export function subscribeToasts(
  pluginName: string,
  listener: (toasts: ToastNotification[]) => void,
): () => void {
  const store = toastStore(pluginName);
  store.listeners.add(listener);
  return () => {
    store.listeners.delete(listener);
  };
}

function notifyToasts(pluginName: string): void {
  const store = toastStore(pluginName);
  const snapshot = [...store.toasts];
  for (const listener of store.listeners) {
    listener(snapshot);
  }
}

function createToaster(pluginName: string): DeckyApi['toaster'] {
  return {
    toast(data) {
      const store = toastStore(pluginName);
      const notification: ToastNotification = {
        data,
        dismiss: () => {
          store.toasts = store.toasts.filter((toast) => toast !== notification);
          notifyToasts(pluginName);
        },
      };
      store.toasts.push(notification);
      notifyToasts(pluginName);
      return notification;
    },
  };
}

function createRouterHook(): RouterHook {
  const routes = new Map<string, ComponentType<never>>();
  const patches = new Map<string, Set<unknown>>();
  const globals = new Map<string, ComponentType<never>>();
  return {
    addRoute(path, component) {
      routes.set(path, component);
    },
    addPatch(path, patch) {
      const set = patches.get(path) ?? new Set();
      set.add(patch);
      patches.set(path, set);
      return patch;
    },
    addGlobalComponent(name, component) {
      globals.set(name, component);
    },
    removeRoute(path) {
      routes.delete(path);
    },
    removePatch(path, patch) {
      patches.get(path)?.delete(patch);
    },
    removeGlobalComponent(name) {
      globals.delete(name);
    },
    async waitForUnlock() {
      // The clean-room loader does not patch Steam's router, so nothing can be locked there.
    },
  };
}

function evaluateInTab(runAsync: boolean, code: string): Promise<unknown> {
  void runAsync;
  try {
    // The plugin already runs in Steam's page; this is the same trust boundary as Decky's API.
    const fn = new Function(code) as () => unknown;
    return Promise.resolve(fn());
  } catch (err: unknown) {
    return Promise.reject(err);
  }
}

function injectCss(pluginName: string, style: string): string {
  const next = cssIds.get(pluginName) ?? 0;
  cssIds.set(pluginName, next + 1);
  const id = `zerodeck-css-${pluginName}-${next}`;
  const element = document.createElement('style');
  element.id = id;
  element.textContent = style;
  document.head.appendChild(element);
  return id;
}

function removeCss(id: string): void {
  document.getElementById(id)?.remove();
}

function externalResourceUrl(url: string, options: LoaderApiOptions): string {
  const token = options.getToken() ?? '';
  return `${options.origin}/fetch?auth=${encodeURIComponent(token)}&fetch_url=${encodeURIComponent(url)}`;
}

function fetchNoCors(
  input: string,
  init: (RequestInit & { excludedHeaders?: string[] }) | undefined,
  options: LoaderApiOptions,
): Promise<Response> {
  const { headers: rawHeaders = {}, excludedHeaders, ...rest } = init ?? {};
  const headers = new Headers(rawHeaders);
  for (const [name, value] of Object.entries(rawHeaders as Record<string, string>)) {
    headers.delete(name);
    headers.set(`X-Decky-Header-${name}`, value);
  }
  if (excludedHeaders && excludedHeaders.length > 0) {
    headers.set('X-Decky-Fetch-Excluded-Headers', excludedHeaders.join(', '));
  }
  return fetch(externalResourceUrl(input, options), {
    ...rest,
    credentials: 'include',
    headers,
  });
}

function openFilePickerUnsupported(): Promise<never> {
  return Promise.reject(new UnsupportedFeatureError('openFilePicker'));
}

/**
 * Builds the API object returned to `@decky/api`'s `connect(version, pluginName)` call. The
 * returned `dispose` removes this plugin's WebSocket listener on unload.
 */
export function createLoaderApi(
  router: RouterLike,
  pluginName: string,
  options: LoaderApiOptions,
  requestedVersion = 2,
): { api: DeckyApi; dispose: () => void } {
  const listeners = new Map<string, Set<(...args: unknown[]) => unknown>>();
  const onPluginEvent = (payload: unknown) => {
    if (!payload || typeof payload !== 'object') return;
    const { plugin, event, args } = payload as {
      plugin?: string;
      event?: string;
      args?: unknown[];
    };
    if (plugin !== pluginName || !event) return;
    for (const listener of listeners.get(event) ?? []) {
      try {
        void Promise.resolve(listener(...(args ?? []))).catch((err: unknown) => {
          console.error(`[ZeroDeck] error in plugin event ${event}`, err);
        });
      } catch (err: unknown) {
        console.error(`[ZeroDeck] error in plugin event ${event}`, err);
      }
    }
  };
  router.addEventListener('loader/plugin_event', onPluginEvent);

  const api: DeckyApi = {
    _version: Math.min(requestedVersion, 2),
    call<Args extends unknown[] = [], Return = void>(
      route: string,
      ...args: Args
    ): Promise<Return> {
      return router.call<[string, string, ...Args], Return>(
        'loader/call_plugin_method',
        pluginName,
        route,
        ...args,
      );
    },
    callable:
      <Args extends unknown[] = [], Return = void>(route: string) =>
      (...args: Args): Promise<Return> =>
        router.call<[string, string, ...Args], Return>(
          'loader/call_plugin_method',
          pluginName,
          route,
          ...args,
        ),
    addEventListener(event, listener) {
      const set = listeners.get(event) ?? new Set();
      set.add(listener);
      listeners.set(event, set);
      return listener;
    },
    removeEventListener(event, listener) {
      listeners.get(event)?.delete(listener);
    },
    routerHook: createRouterHook(),
    toaster: createToaster(pluginName),
    openFilePicker: openFilePickerUnsupported,
    async executeInTab(tab, runAsync, code) {
      void tab;
      try {
        return { success: true, result: await evaluateInTab(runAsync, code) };
      } catch (err: unknown) {
        return { success: false, result: err instanceof Error ? err.message : String(err) };
      }
    },
    injectCssIntoTab(tab, style) {
      void tab;
      return injectCss(pluginName, style);
    },
    removeCssFromTab(tab, cssId) {
      void tab;
      removeCss(cssId);
    },
    fetchNoCors(input, init) {
      return fetchNoCors(input, init, options);
    },
    getExternalResourceURL(url) {
      return externalResourceUrl(url, options);
    },
    useQuickAccessVisible,
  };

  return {
    api,
    dispose: () => {
      router.removeEventListener('loader/plugin_event', onPluginEvent);
      listeners.clear();
    },
  };
}

/** API object passed to legacy IIFE plugin bundles. */
export interface LegacyPluginApi {
  routerHook: RouterHook;
  toaster: DeckyApi['toaster'];
  callPluginMethod(method: string, args?: Record<string, unknown>): Promise<unknown>;
  callServerMethod(method: string, args?: Record<string, unknown>): Promise<unknown>;
  openFilePicker(...args: unknown[]): Promise<never>;
  openFilePickerV2(...args: unknown[]): Promise<never>;
  fetchNoCors(input: string, init?: RequestInit): Promise<Response>;
  executeInTab(
    tab: string,
    runAsync: boolean,
    code: string,
  ): Promise<{ success: boolean; result: unknown }>;
  injectCssIntoTab(tab: string, style: string): string;
  removeCssFromTab(tab: string, cssId: string): void;
}

export function createLegacyPluginApi(
  router: RouterLike,
  pluginName: string,
  options: LoaderApiOptions,
): LegacyPluginApi {
  return {
    routerHook: createRouterHook(),
    toaster: createToaster(pluginName),
    callPluginMethod(method, args = {}) {
      return router.call('loader/call_legacy_plugin_method', pluginName, method, args);
    },
    callServerMethod(method, args = {}) {
      return router.call('utilities/_call_legacy_utility', method, args);
    },
    openFilePicker: openFilePickerUnsupported,
    openFilePickerV2: openFilePickerUnsupported,
    fetchNoCors(input, init) {
      return fetchNoCors(input, init, options);
    },
    async executeInTab(tab, runAsync, code) {
      void tab;
      try {
        return { success: true, result: await evaluateInTab(runAsync, code) };
      } catch (err: unknown) {
        return { success: false, result: err instanceof Error ? err.message : String(err) };
      }
    },
    injectCssIntoTab(tab, style) {
      void tab;
      return injectCss(pluginName, style);
    },
    removeCssFromTab(tab, cssId) {
      void tab;
      removeCss(cssId);
    },
  };
}

interface DeckyApiConnection {
  connect(version: number, pluginName: string): DeckyApi;
}

declare global {
  interface Window {
    __DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit?: DeckyApiConnection;
  }
}

const apiDisposers = new Map<string, () => void>();

type LoaderGlobal = typeof globalThis & {
  __DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit?: DeckyApiConnection;
};

/**
 * Installs the global `@decky/api` connection hook. It must be present before any plugin module is
 * imported, because `@decky/api` reads it during module evaluation.
 */
export function installDeckyApi(
  factory: (version: number, pluginName: string) => { api: DeckyApi; dispose: () => void },
): void {
  const scope = globalThis as LoaderGlobal;
  scope.__DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit = {
    connect(version, pluginName) {
      apiDisposers.get(pluginName)?.();
      const connection = factory(version, pluginName);
      apiDisposers.set(pluginName, connection.dispose);
      return connection.api;
    },
  };
}

/** Releases the API listeners registered by one plugin. */
export function disposeDeckyApi(pluginName: string): void {
  apiDisposers.get(pluginName)?.();
  apiDisposers.delete(pluginName);
}
