import assert from 'node:assert/strict';
import test from 'node:test';

import {
  createLegacyPluginApi,
  createLoaderApi,
  disposeDeckyApi,
  installDeckyApi,
  type RouterLike,
} from '../src/decky/api.ts';

// `@decky/api` reads this global through `window`; Node tests have no browser window.
(globalThis as { window?: unknown }).window = globalThis;

class FakeRouter implements RouterLike {
  calls: Array<{ route: string; args: unknown[] }> = [];
  private listeners = new Map<string, Set<(...args: unknown[]) => unknown>>();

  async call<Args extends unknown[] = [], Return = unknown>(
    route: string,
    ...args: Args
  ): Promise<Return> {
    this.calls.push({ route, args });
    return 'result' as Return;
  }

  addEventListener(event: string, listener: (...args: unknown[]) => unknown): unknown {
    const set = this.listeners.get(event) ?? new Set();
    set.add(listener);
    this.listeners.set(event, set);
    return listener;
  }

  removeEventListener(event: string, listener: (...args: unknown[]) => unknown): void {
    this.listeners.get(event)?.delete(listener);
  }

  emit(event: string, ...args: unknown[]): void {
    for (const listener of this.listeners.get(event) ?? []) {
      listener(...args);
    }
  }
}

const options = {
  origin: 'http://127.0.0.1:1337',
  getToken: () => 'tok',
};

test('call and callable route plugin methods through loader/call_plugin_method', async () => {
  const router = new FakeRouter();
  const { api } = createLoaderApi(router, 'Demo', options);

  assert.equal(await api.call('ping', 1), 'result');
  assert.equal(await api.callable<[number], string>('ping')(2), 'result');
  assert.deepEqual(router.calls, [
    { route: 'loader/call_plugin_method', args: ['Demo', 'ping', 1] },
    { route: 'loader/call_plugin_method', args: ['Demo', 'ping', 2] },
  ]);
});

test('plugin events are filtered by plugin and disposed with the API', async () => {
  const router = new FakeRouter();
  const { api, dispose } = createLoaderApi(router, 'Demo', options);
  const received: unknown[] = [];
  api.addEventListener('tick', (...args) => {
    received.push(args);
  });

  router.emit('loader/plugin_event', { plugin: 'Other', event: 'tick', args: [1] });
  router.emit('loader/plugin_event', { plugin: 'Demo', event: 'tick', args: [2, 3] });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(received, [[2, 3]]);

  dispose();
  router.emit('loader/plugin_event', { plugin: 'Demo', event: 'tick', args: [4] });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(received, [[2, 3]]);
});

test('external resource URLs use the authenticated fetch proxy', () => {
  const router = new FakeRouter();
  const { api } = createLoaderApi(router, 'Demo', options);
  assert.equal(
    api.getExternalResourceURL('https://example.com/a b'),
    'http://127.0.0.1:1337/fetch?auth=tok&fetch_url=https%3A%2F%2Fexample.com%2Fa%20b',
  );
});

test('legacy API dispatches legacy plugin methods', async () => {
  const router = new FakeRouter();
  const api = createLegacyPluginApi(router, 'Old', options);
  await api.callPluginMethod('ping', { value: 1 });
  assert.deepEqual(router.calls, [
    {
      route: 'loader/call_legacy_plugin_method',
      args: ['Old', 'ping', { value: 1 }],
    },
  ]);
});

test('installDeckyApi connects each plugin and disposeDeckyApi removes its listener', async () => {
  const router = new FakeRouter();
  installDeckyApi((version, pluginName) => createLoaderApi(router, pluginName, options, version));
  const api =
    window.__DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit?.connect(
      2,
      'Demo',
    );
  assert.equal(api?._version, 2);

  const received: unknown[] = [];
  api?.addEventListener('tick', (...args) => {
    received.push(args);
  });
  disposeDeckyApi('Demo');
  router.emit('loader/plugin_event', { plugin: 'Demo', event: 'tick', args: [1] });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(received, []);
});
