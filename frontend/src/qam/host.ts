import { createElement } from 'react';

import type { PluginLoader } from '../decky/plugin-loader.ts';
import type { WSRouter } from '../wsrouter.ts';
import { QamApp } from './app.ts';

interface SteamReactDom {
  createRoot?: (container: Element) => { render: (node: unknown) => void };
}

export interface QamHostOptions {
  router: WSRouter;
  loader: PluginLoader;
  origin: string;
  getToken: () => string | undefined;
  /** Called once the panel host is mounted, so CEF can report `qam_ready`. */
  onMounted?: () => void;
}

/**
 * Mounts the ZeroDeck panel as a page overlay. The Steam QAM tab adapter is intentionally separate:
 * it requires a hardware-qualified React tree patch, while this host works without Steam internals.
 */
export class QamHost {
  private readonly options: QamHostOptions;
  private mounted = false;

  constructor(options: QamHostOptions) {
    this.options = options;
  }

  mount(): boolean {
    if (this.mounted) return true;
    const reactDom = (globalThis as { SP_REACTDOM?: SteamReactDom }).SP_REACTDOM;
    if (!reactDom?.createRoot) {
      console.warn('[ZeroDeck] Steam ReactDOM is unavailable; QAM host not mounted');
      return false;
    }
    const container = document.createElement('div');
    container.id = 'zerodeck-qam-host';
    document.body.appendChild(container);
    const root = reactDom.createRoot(container);
    root.render(
      createElement(QamApp, {
        router: this.options.router,
        loader: this.options.loader,
        origin: this.options.origin,
        getToken: this.options.getToken,
      }),
    );
    window.addEventListener('keydown', this.onKeyDown);
    this.mounted = true;
    this.options.onMounted?.();
    return true;
  }

  private readonly onKeyDown = (event: KeyboardEvent) => {
    if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'z') {
      event.preventDefault();
      window.dispatchEvent(new CustomEvent('zerodeck:qam-toggle'));
    }
  };
}
