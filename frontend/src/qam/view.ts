import { createElement, type ReactNode } from 'react';

export interface QamPluginEntry {
  name: string;
  version?: string | null;
  state: string;
  disabled: boolean;
  passive?: boolean;
  content?: ReactNode;
  error?: string;
}

export interface QamViewProps {
  open: boolean;
  plugins: QamPluginEntry[];
  selected?: string;
  errors: Array<{ name: string; error: string }>;
  cefState: string;
  onToggleOpen: () => void;
  onSelect: (name: string) => void;
  onTogglePlugin: (plugin: QamPluginEntry) => void;
  onReload: (name: string) => void;
}

const QAM_CSS = `
.zerodeck-qam-toggle{position:fixed;right:16px;bottom:16px;z-index:10002;background:#1a9fff;color:#fff;border:0;border-radius:16px;padding:8px 14px;font:inherit;cursor:pointer}
.zerodeck-qam-panel{position:fixed;right:0;top:0;bottom:0;width:360px;z-index:10001;background:#1b2838;color:#c6d4df;box-shadow:-4px 0 12px rgba(0,0,0,.5);display:flex;flex-direction:column;font:14px/1.35 sans-serif}
.zerodeck-qam-header{display:flex;align-items:center;justify-content:space-between;padding:12px;border-bottom:1px solid #2a475e}
.zerodeck-qam-list{display:flex;gap:4px;overflow-x:auto;padding:8px 12px;border-bottom:1px solid #2a475e}
.zerodeck-qam-list button{background:#2a475e;color:#c6d4df;border:0;border-radius:4px;padding:6px 8px;cursor:pointer;white-space:nowrap}
.zerodeck-qam-list button[aria-pressed="true"]{background:#1a9fff;color:#fff}
.zerodeck-qam-content{flex:1;overflow:auto;padding:12px}
.zerodeck-qam-footer{padding:8px 12px;border-top:1px solid #2a475e;font-size:12px;opacity:.8}
.zerodeck-qam-error{color:#ffb3b3;margin-bottom:8px}
.zerodeck-qam-actions{display:flex;gap:8px;margin-bottom:8px}
.zerodeck-qam-actions button,.zerodeck-qam-header button{background:#2a475e;color:#c6d4df;border:0;border-radius:4px;padding:4px 8px;cursor:pointer}
`;

export function QamView(props: QamViewProps): ReactNode {
  const selected =
    props.plugins.find((plugin) => plugin.name === props.selected) ?? props.plugins[0];
  return createElement(
    'div',
    { className: 'zerodeck-qam-root' },
    createElement('style', null, QAM_CSS),
    props.open
      ? createElement(
          'section',
          {
            className: 'zerodeck-qam-panel',
            'data-testid': 'zerodeck-qam-panel',
            'aria-label': 'ZeroDeck Quick Access Menu',
          },
          createElement(
            'header',
            { className: 'zerodeck-qam-header' },
            createElement('strong', null, 'ZeroDeck'),
            createElement(
              'button',
              { type: 'button', onClick: props.onToggleOpen, 'aria-label': 'Close ZeroDeck menu' },
              'Close',
            ),
          ),
          createElement(
            'nav',
            { className: 'zerodeck-qam-list', 'aria-label': 'Plugins' },
            props.plugins.map((plugin) =>
              createElement(
                'button',
                {
                  type: 'button',
                  key: plugin.name,
                  'aria-pressed': selected?.name === plugin.name,
                  onClick: () => props.onSelect(plugin.name),
                },
                plugin.name,
              ),
            ),
          ),
          createElement(
            'div',
            { className: 'zerodeck-qam-content' },
            selected
              ? createElement(
                  'article',
                  { 'data-testid': 'zerodeck-qam-plugin' },
                  createElement(
                    'div',
                    { className: 'zerodeck-qam-actions' },
                    createElement(
                      'button',
                      { type: 'button', onClick: () => props.onTogglePlugin(selected) },
                      selected.disabled ? 'Enable' : 'Disable',
                    ),
                    createElement(
                      'button',
                      { type: 'button', onClick: () => props.onReload(selected.name) },
                      'Reload',
                    ),
                    createElement(
                      'span',
                      { style: { opacity: 0.7 } },
                      `${selected.version ?? 'unknown'} · ${selected.state}`,
                    ),
                  ),
                  selected.error
                    ? createElement(
                        'p',
                        { className: 'zerodeck-qam-error', 'data-testid': 'zerodeck-plugin-error' },
                        `Failed to load: ${selected.error}`,
                      )
                    : selected.disabled
                      ? createElement('p', null, 'This plugin is disabled.')
                      : (selected.content ?? createElement('p', null, 'This plugin has no panel.')),
                )
              : createElement('p', null, 'No plugins installed.'),
          ),
          props.errors.length > 0
            ? createElement(
                'div',
                { className: 'zerodeck-qam-content', 'data-testid': 'zerodeck-qam-errors' },
                props.errors.map((error) =>
                  createElement(
                    'p',
                    { className: 'zerodeck-qam-error', key: error.name },
                    `${error.name}: ${error.error}`,
                  ),
                ),
              )
            : null,
          createElement(
            'footer',
            { className: 'zerodeck-qam-footer', 'data-testid': 'zerodeck-qam-cef' },
            `Steam integration: ${props.cefState}`,
          ),
        )
      : createElement(
          'button',
          {
            type: 'button',
            className: 'zerodeck-qam-toggle',
            'data-testid': 'zerodeck-qam-toggle',
            onClick: props.onToggleOpen,
          },
          'ZeroDeck',
        ),
  );
}
