import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

import { MessageType, PyError, type WebSocketLike, WSRouter } from '../src/wsrouter.ts';

class MockWebSocket implements WebSocketLike {
  readyState = 1;
  sent: string[] = [];
  private listeners = new Map<string, Set<(event: unknown) => void>>();

  addEventListener(type: string, listener: (event: unknown) => void): void {
    const existing = this.listeners.get(type) ?? new Set();
    existing.add(listener);
    this.listeners.set(type, existing);
  }

  send(data: string): void {
    this.sent.push(data);
  }

  emit(type: string, event: unknown): void {
    for (const listener of this.listeners.get(type) ?? []) {
      listener(event);
    }
  }
}

test('WSRouter.call sends CALL and resolves REPLY with RECEIVED_RESPONSE ack', async () => {
  const sockets: MockWebSocket[] = [];
  const router = new WSRouter({
    baseUrl: 'ws://127.0.0.1:1337',
    createSocket: () => {
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const connPromise = router.connect('tok-1');
  sockets[0].emit('open', {});
  await connPromise;

  // Initial connect sends empty FULL_SYNC
  assert.equal(JSON.parse(sockets[0].sent[0]).type, MessageType.FullSync);

  const callPromise = router.call<[string], string>('utilities/ping', 'hi');
  const sentCall = JSON.parse(sockets[0].sent[1]);
  assert.equal(sentCall.type, MessageType.Call);
  assert.equal(sentCall.id, 1);
  assert.equal(sentCall.route, 'utilities/ping');
  assert.deepEqual(sentCall.args, ['hi']);

  sockets[0].emit('message', {
    data: JSON.stringify({ type: MessageType.Reply, id: 1, result: 'pong' }),
  });

  const result = await callPromise;
  assert.equal(result, 'pong');
  assert.deepEqual(JSON.parse(sockets[0].sent[2]), {
    type: MessageType.ReceivedResponse,
    id: 1,
  });
});

test('WSRouter rejects with PyError on ERROR (-1)', async () => {
  const sockets: MockWebSocket[] = [];
  const router = new WSRouter({
    baseUrl: 'ws://127.0.0.1:1337',
    createSocket: () => {
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const connPromise = router.connect('tok-2');
  sockets[0].emit('open', {});
  await connPromise;

  const callPromise = router.call('broken/route');
  sockets[0].emit('message', {
    data: JSON.stringify({
      type: MessageType.Error,
      id: 1,
      error: { name: 'RouteNotFoundError', error: 'missing', traceback: null },
    }),
  });

  await assert.rejects(callPromise, (err: unknown) => {
    assert.ok(err instanceof PyError);
    assert.equal(err.message, 'missing');
    return true;
  });
});

test('WSRouter dispatches EVENT (5) and replays in-flight CALLs on FULL_SYNC (4)', async () => {
  const sockets: MockWebSocket[] = [];
  const router = new WSRouter({
    baseUrl: 'ws://127.0.0.1:1337',
    createSocket: () => {
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const conn1 = router.connect('tok-3');
  sockets[0].emit('open', {});
  await conn1;

  const receivedEvents: unknown[] = [];
  router.addEventListener('loader/plugin_event', (payload) => {
    receivedEvents.push(payload);
  });

  sockets[0].emit('message', {
    data: JSON.stringify({
      type: MessageType.Event,
      event: 'loader/plugin_event',
      args: [{ plugin: 'Demo' }],
    }),
  });
  assert.deepEqual(receivedEvents, [{ plugin: 'Demo' }]);

  const pendingCall = router.call('slow/method', 99);
  const conn2 = router.connect('tok-3');
  sockets[1].emit('open', {});
  await conn2;

  const fullSync = JSON.parse(sockets[1].sent[0]);
  assert.equal(fullSync.type, MessageType.FullSync);
  assert.equal(fullSync.messages.length, 1);
  assert.equal(fullSync.messages[0].route, 'slow/method');

  sockets[1].emit('message', {
    data: JSON.stringify({
      type: MessageType.Reply,
      id: 1,
      result: 'recovered',
    }),
  });
  assert.equal(await pendingCall, 'recovered');
});

test('WSRouter rejects pending call on DISCARD (2) and survives malformed JSON', async () => {
  const sockets: MockWebSocket[] = [];
  const router = new WSRouter({
    baseUrl: 'ws://127.0.0.1:1337',
    createSocket: () => {
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const conn = router.connect('tok-4');
  sockets[0].emit('open', {});
  await conn;

  sockets[0].emit('message', { data: '{invalid json' });

  const discardedCall = router.call('plugin/unloaded');
  sockets[0].emit('message', {
    data: JSON.stringify({ type: MessageType.Discard, id: 1 }),
  });

  await assert.rejects(discardedCall, /discarded/i);
});

test('WSRouter keeps dispatching EVENT to other listeners when one throws synchronously', async () => {
  const sockets: MockWebSocket[] = [];
  const router = new WSRouter({
    createSocket: () => {
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const conn = router.connect('tok-5');
  sockets[0].emit('open', {});
  await conn;

  const received: unknown[] = [];
  router.addEventListener('loader/tick', () => {
    throw new Error('listener bug');
  });
  router.addEventListener('loader/tick', (payload) => {
    received.push(payload);
  });

  const originalError = console.error;
  console.error = () => {};
  try {
    assert.doesNotThrow(() => {
      sockets[0].emit('message', {
        data: JSON.stringify({ type: MessageType.Event, event: 'loader/tick', args: [7] }),
      });
    });
  } finally {
    console.error = originalError;
  }
  assert.deepEqual(received, [7]);
});

test('WSRouter fetches a fresh auth token before reconnecting', async () => {
  const sockets: MockWebSocket[] = [];
  const urls: string[] = [];
  const router = new WSRouter({
    reconnectDelayMs: 0,
    refreshAuthToken: async () => 'tok-new',
    createSocket: (url) => {
      urls.push(url);
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const conn = router.connect('tok-old');
  sockets[0].emit('open', {});
  await conn;

  sockets[0].emit('close', {});
  await new Promise((resolve) => setTimeout(resolve, 10));

  assert.equal(urls.length, 2);
  assert.match(urls[0], /auth=tok-old$/);
  assert.match(urls[1], /auth=tok-new$/);
});

test('WSRouter backs off exponentially between failed reconnects and resets after open', async () => {
  const sockets: MockWebSocket[] = [];
  const delays: number[] = [];
  const pending: (() => void)[] = [];
  const router = new WSRouter({
    reconnectDelayMs: 100,
    maxReconnectDelayMs: 1000,
    schedule: (callback, delayMs) => {
      delays.push(delayMs);
      pending.push(callback);
    },
    createSocket: () => {
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const conn = router.connect('tok-6');
  sockets[0].emit('open', {});
  await conn;

  // The daemon goes away: every reconnect attempt closes without ever opening.
  sockets[0].emit('close', {});
  for (let attempt = 1; attempt <= 5; attempt++) {
    pending.shift()?.();
    await new Promise((resolve) => setTimeout(resolve, 0));
    sockets[attempt].emit('close', {});
  }
  assert.deepEqual(delays, [100, 200, 400, 800, 1000, 1000]);

  // Once a reconnect succeeds, the next drop starts from the base delay again.
  pending.shift()?.();
  await new Promise((resolve) => setTimeout(resolve, 0));
  sockets[6].emit('open', {});
  sockets[6].emit('close', {});
  assert.equal(delays.at(-1), 100);
});

test('WSRouter.start keeps retrying until the daemon hands out a token', async () => {
  const urls: string[] = [];
  const delays: number[] = [];
  const pending: (() => void)[] = [];
  let tokenRequests = 0;
  const router = new WSRouter({
    reconnectDelayMs: 100,
    refreshAuthToken: async () => {
      tokenRequests += 1;
      if (tokenRequests < 3) throw new Error('connection refused');
      return 'tok-late';
    },
    schedule: (callback, delayMs) => {
      delays.push(delayMs);
      pending.push(callback);
    },
    createSocket: (url) => {
      urls.push(url);
      return new MockWebSocket();
    },
  });

  const originalWarn = console.warn;
  console.warn = () => {};
  try {
    router.start();
    for (let i = 0; i < 2; i++) {
      await new Promise((resolve) => setTimeout(resolve, 0));
      pending.shift()?.();
    }
    await new Promise((resolve) => setTimeout(resolve, 0));
  } finally {
    console.warn = originalWarn;
  }

  assert.equal(tokenRequests, 3);
  assert.deepEqual(delays, [100, 200]);
  assert.equal(urls.length, 1);
  assert.match(urls[0], /auth=tok-late$/);
});

test('WSRouter.start requires refreshAuthToken', () => {
  assert.throws(() => new WSRouter().start(), /refreshAuthToken/);
});

test('WSRouter rejects calls that exceed callTimeoutMs and stops replaying them', async () => {
  const sockets: MockWebSocket[] = [];
  const router = new WSRouter({
    callTimeoutMs: 10,
    createSocket: () => {
      const ws = new MockWebSocket();
      sockets.push(ws);
      return ws;
    },
  });

  const conn = router.connect('tok-7');
  sockets[0].emit('open', {});
  await conn;

  await assert.rejects(router.call('slow/never'), /timed out after 10ms/);

  const conn2 = router.connect('tok-7');
  sockets[1].emit('open', {});
  await conn2;
  assert.deepEqual(JSON.parse(sockets[1].sent[0]).messages, []);
});

test('vite steamExternalsPlugin exports named JSX and React bindings', async () => {
  const { renderSteamGlobalModule } = await import('../vite.config.ts');
  const jsxShim = renderSteamGlobalModule('react/jsx-runtime', 'SP_JSX');
  assert.match(jsxShim, /export const jsx\b/);
  assert.match(jsxShim, /export const jsxs\b/);
  assert.match(jsxShim, /export const Fragment\b/);

  const reactShim = renderSteamGlobalModule('react', 'SP_REACT');
  assert.match(reactShim, /export const useState\b/);
  assert.match(reactShim, /export default\b/);

  const viteSrc = readFileSync(new URL('../vite.config.ts', import.meta.url), 'utf8');
  assert.match(viteSrc, /renderSteamGlobalModule/);
});
