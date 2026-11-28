import assert from 'node:assert/strict';
import test from 'node:test';

import { type ComponentType, createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import { DFL } from '../src/decky/dfl.ts';

test('known DFL components are available', () => {
  for (const name of [
    'PanelSection',
    'PanelSectionRow',
    'ButtonItem',
    'ToggleField',
    'ModalRoot',
  ]) {
    assert.equal(typeof DFL[name], 'function', name);
  }
});

test('unknown DFL components render an explicit diagnostic instead of crashing', () => {
  const Missing = DFL.NotARealComponent as ComponentType<Record<string, unknown>>;
  const html = renderToStaticMarkup(createElement(Missing, {}));
  assert.match(html, /NotARealComponent/);
  assert.match(html, /not implemented/);
});
