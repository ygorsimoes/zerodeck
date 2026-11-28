import assert from 'node:assert/strict';
import test from 'node:test';
import type { RouterLike } from '../src/decky/api.ts';
import { PluginLoader, type PluginSummary } from '../src/decky/plugin-loader.ts';

class FakeRouter implements RouterLike {
  calls: Array<{ route: string; args: unknown[] }> = [];

  async call<Args extends unknown[] = [], Return = unknown>(
    route: string,
    ...args: Args
  ): Promise<Return> {
    this.calls.push({ route, args });
    return undefined as Return;
  }

  addEventListener(): unknown {
    return undefined;
  }

  removeEventListener(): void {}
}

function summary(overrides: Partial<PluginSummary> = {}): PluginSummary {
  return {
    name: 'Demo',
    version: '1.2.3',
    load_type: 1,
    disabled: false,
    passive: false,
    state: 'Running',
    ...overrides,
  };
}

function loaderFor(importModule: (url: string) => Promise<{ default?: unknown }>) {
  const router = new FakeRouter();
  const loader = new PluginLoader({
    origin: 'http://127.0.0.1:1337',
    router,
    getToken: () => 'tok',
    importModule,
  });
  return { loader, router };
}

test('ESM plugins load from the generation URL and dismount once', async () => {
  const urls: string[] = [];
  let dismounts = 0;
  const { loader } = loaderFor(async (url) => {
    urls.push(url);
    return {
      default: () => ({
        content: 'panel',
        onDismount: () => {
          dismounts += 1;
        },
      }),
    };
  });

  await loader.sync([summary()]);
  assert.equal(loader.plugins().length, 1);
  assert.match(urls[0], /\/plugins\/Demo\/dist\/index\.js\?generation=1\.2\.3-1-0$/);

  loader.unload('Demo');
  assert.equal(dismounts, 1);
  assert.equal(loader.plugins().length, 0);
});

test('legacy IIFE plugins receive the legacy API object', async () => {
  const { loader, router } = loaderFor(async () => {
    throw new Error('ESM import must not be used for legacy plugins');
  });
  const loaderWithFetch = new PluginLoader({
    origin: 'http://127.0.0.1:1337',
    router,
    getToken: () => 'tok',
    importModule: async () => {
      throw new Error('unexpected ESM import');
    },
    fetchText: async (url, headers) => {
      assert.match(url, /\/plugins\/Old\/frontend_bundle$/);
      assert.equal(headers['X-Decky-Auth'], 'tok');
      return '((api) => ({ content: typeof api.callPluginMethod }))';
    },
  });

  await loaderWithFetch.sync([summary({ name: 'Old', version: null, load_type: 0 })]);
  assert.equal(loaderWithFetch.plugins()[0]?.definition.content, 'function');
  assert.equal(loader.plugins().length, 0);
});

test('one failing plugin does not prevent the others from loading', async () => {
  const { loader } = loaderFor(async (url) => {
    if (url.includes('Bad')) throw new Error('load failed');
    return { default: () => ({ content: 'ok' }) };
  });

  await loader.sync([summary({ name: 'Bad' }), summary({ name: 'Good' })]);
  assert.deepEqual(
    loader.plugins().map((plugin) => plugin.name),
    ['Good'],
  );
  assert.deepEqual(loader.loadErrors(), [{ name: 'Bad', error: 'Error: load failed' }]);
});

test('sync unloads disabled plugins and keeps unchanged ones untouched', async () => {
  const urls: string[] = [];
  const { loader } = loaderFor(async (url) => {
    urls.push(url);
    return { default: () => ({ content: 'ok' }) };
  });

  await loader.sync([summary()]);
  await loader.sync([summary()]);
  assert.equal(urls.length, 1);

  await loader.sync([summary({ disabled: true })]);
  assert.equal(loader.plugins().length, 0);
});

test('reload uses a fresh generation URL', async () => {
  const urls: string[] = [];
  const { loader } = loaderFor(async (url) => {
    urls.push(url);
    return { default: () => ({ content: 'ok' }) };
  });

  await loader.sync([summary()]);
  await loader.reload('Demo');
  assert.equal(urls.length, 2);
  assert.match(urls[1], /generation=1\.2\.3-1-1$/);
});
