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
