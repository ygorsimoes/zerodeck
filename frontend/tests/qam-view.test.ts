import assert from 'node:assert/strict';
import test from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

import { QamView } from '../src/qam/view.ts';

const baseProps = {
  selected: 'Demo',
  errors: [],
  cefState: 'active',
  onToggleOpen: () => {},
  onSelect: () => {},
  onTogglePlugin: () => {},
  onReload: () => {},
};

test('closed QAM renders only the ZeroDeck toggle', () => {
  const html = renderToStaticMarkup(
    createElement(QamView, {
      ...baseProps,
      open: false,
      plugins: [{ name: 'Demo', version: '1.0', state: 'Running', disabled: false }],
    }),
  );
  assert.match(html, /zerodeck-qam-toggle/);
  assert.doesNotMatch(html, /data-testid="zerodeck-qam-panel"/);
});

test('open QAM lists plugins, selected content, state and CEF health', () => {
  const html = renderToStaticMarkup(
    createElement(QamView, {
      ...baseProps,
      open: true,
      plugins: [
        {
          name: 'Demo',
          version: '1.0',
          state: 'Running',
          disabled: false,
          content: 'Demo panel content',
        },
        { name: 'Off', version: '2.0', state: 'Stopped', disabled: true },
      ],
    }),
  );
  assert.match(html, /zerodeck-qam-panel/);
  assert.match(html, /Demo panel content/);
  assert.match(html, /Off/);
  assert.match(html, /Steam integration: active/);
});

test('plugin load errors and disabled state are visible', () => {
  const html = renderToStaticMarkup(
    createElement(QamView, {
      ...baseProps,
      open: true,
      selected: 'Broken',
      plugins: [
        {
          name: 'Broken',
          version: '1.0',
          state: 'Failed',
          disabled: false,
          error: 'import failed',
        },
      ],
      errors: [{ name: 'Broken', error: 'import failed' }],
    }),
  );
  assert.match(html, /Failed to load: import failed/);
  assert.match(html, /zerodeck-qam-errors/);
});
