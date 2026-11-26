import assert from 'node:assert/strict';
import test from 'node:test';

import { daemonOrigin, FALLBACK_ORIGIN, webSocketBase } from '../src/origin.ts';

test('daemonOrigin uses the origin of a bundle served over HTTP', () => {
  assert.equal(daemonOrigin('http://127.0.0.1:4242/frontend/index.js'), 'http://127.0.0.1:4242');
  assert.equal(
    daemonOrigin('https://deck.local:8443/frontend/index.js'),
    'https://deck.local:8443',
  );
});

test('daemonOrigin falls back when the bundle URL is not HTTP(S)', () => {
  assert.equal(daemonOrigin('file:///x/index.js'), FALLBACK_ORIGIN);
  assert.equal(daemonOrigin(undefined), FALLBACK_ORIGIN);
  assert.equal(daemonOrigin('not a url'), FALLBACK_ORIGIN);
  assert.equal(FALLBACK_ORIGIN, 'http://127.0.0.1:1337');
});

test('webSocketBase swaps the scheme for its WebSocket counterpart', () => {
  assert.equal(webSocketBase('http://h:1'), 'ws://h:1');
  assert.equal(webSocketBase('https://h:1'), 'wss://h:1');
});
