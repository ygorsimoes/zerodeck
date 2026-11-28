import { Component, type ComponentType, createElement, type ReactNode } from 'react';
import { useQuickAccessVisible } from './visibility.ts';

type Props = Record<string, unknown> & { children?: ReactNode };

const panelStyle = {
  background: 'rgba(0, 0, 0, 0.2)',
  borderRadius: '4px',
  marginBottom: '8px',
  padding: '8px',
} as const;

function PanelSection(props: Props): ReactNode {
  return createElement(
    'div',
    { className: 'zerodeck-panel-section', style: panelStyle },
    props.title ? createElement('div', { style: { fontWeight: 600 } }, String(props.title)) : null,
    props.children,
  );
}

function PanelSectionRow(props: Props): ReactNode {
  return createElement(
    'div',
    {
      className: 'zerodeck-panel-section-row',
      style: { display: 'flex', alignItems: 'center', gap: '8px', padding: '4px 0' },
    },
    props.children,
  );
}

function ButtonItem(props: Props): ReactNode {
  return createElement(
    'button',
    {
      type: 'button',
      className: 'zerodeck-button-item',
      disabled: Boolean(props.disabled),
      onClick: props.onClick as (() => void) | undefined,
      style: { cursor: 'pointer', padding: '6px 10px' },
    },
    props.children,
  );
}

function ToggleField(props: Props): ReactNode {
  return createElement(
    'label',
    {
      className: 'zerodeck-toggle-field',
      style: { display: 'flex', alignItems: 'center', gap: '8px' },
    },
    createElement('input', {
      type: 'checkbox',
      checked: Boolean(props.checked),
      disabled: Boolean(props.disabled),
      onChange: (event: { target: { checked: boolean } }) =>
        (props.onChange as ((checked: boolean) => void) | undefined)?.(event.target.checked),
    }),
    props.label ? createElement('span', null, String(props.label)) : null,
    props.children,
  );
}

function SliderField(props: Props): ReactNode {
  return createElement(
    'label',
    {
      className: 'zerodeck-slider-field',
      style: { display: 'flex', flexDirection: 'column', gap: '4px' },
    },
    props.label ? createElement('span', null, String(props.label)) : null,
    createElement('input', {
      type: 'range',
      min: Number(props.min ?? 0),
      max: Number(props.max ?? 100),
      step: Number(props.step ?? 1),
      value: Number(props.value ?? 0),
      disabled: Boolean(props.disabled),
      onChange: (event: { target: { value: string } }) =>
        (props.onChange as ((value: number) => void) | undefined)?.(Number(event.target.value)),
    }),
  );
}

function TextField(props: Props): ReactNode {
  return createElement(
    'label',
    { className: 'zerodeck-text-field', style: { display: 'flex', flexDirection: 'column' } },
    props.label ? createElement('span', null, String(props.label)) : null,
    createElement('input', {
      type: 'text',
      value: String(props.value ?? ''),
      placeholder: props.placeholder ? String(props.placeholder) : undefined,
      disabled: Boolean(props.disabled),
      onChange: (event: { target: { value: string } }) =>
        (props.onChange as ((value: string) => void) | undefined)?.(event.target.value),
    }),
  );
}

interface DropdownOption {
  data: unknown;
  label: string;
}

function Dropdown(props: Props): ReactNode {
  const options = (props.rgOptions as DropdownOption[] | undefined) ?? [];
  const selected = props.selectedOption as DropdownOption | undefined;
  return createElement(
    'label',
    { className: 'zerodeck-dropdown' },
    props.label ? createElement('span', null, String(props.label)) : null,
    createElement(
      'select',
      {
        value: selected ? String(selected.data) : '',
        onChange: (event: { target: { value: string } }) => {
          const option = options.find((candidate) => String(candidate.data) === event.target.value);
          if (option) (props.onChange as ((option: DropdownOption) => void) | undefined)?.(option);
        },
      },
      options.map((option) =>
        createElement(
          'option',
          { key: String(option.data), value: String(option.data) },
          option.label,
        ),
      ),
    ),
  );
}

function DropdownItem(props: Props): ReactNode {
  return createElement(
    'div',
    { className: 'zerodeck-dropdown-item', onClick: props.onClick as (() => void) | undefined },
    props.children,
  );
}

function DialogButton(props: Props): ReactNode {
  return createElement(
    'button',
    {
      type: 'button',
      disabled: Boolean(props.disabled),
      onClick: props.onClick as (() => void) | undefined,
      className: 'zerodeck-dialog-button',
    },
    props.children,
  );
}

function Field(props: Props): ReactNode {
  return createElement(
    'div',
    {
      className: 'zerodeck-field',
      style: { display: 'flex', flexDirection: 'column', gap: '2px' },
    },
    props.label ? createElement('span', { style: { opacity: 0.8 } }, String(props.label)) : null,
    props.children,
  );
}

function Focusable(props: Props): ReactNode {
  return createElement(
    'div',
    {
      className: 'zerodeck-focusable',
      role: 'button',
      tabIndex: 0,
      onClick: props.onClick as (() => void) | undefined,
      onKeyDown: (event: { key: string }) => {
        if (event.key === 'Enter' || event.key === ' ') {
          (props.onActivate as (() => void) | undefined)?.();
        }
      },
    },
    props.children,
  );
}

function ProgressBar(props: Props): ReactNode {
  const progress = Math.max(0, Math.min(1, Number(props.nProgress ?? 0)));
  return createElement(
    'div',
    { className: 'zerodeck-progress-bar', style: { background: '#333', height: '8px' } },
    createElement('div', {
      style: { background: '#1a9fff', height: '100%', width: `${progress * 100}%` },
    }),
  );
}

function Spinner(): ReactNode {
  return createElement('span', { className: 'zerodeck-spinner' }, '…');
}

function ModalRoot(props: Props): ReactNode {
  return createElement(
    'div',
    {
      className: 'zerodeck-modal-root',
      style: {
        alignItems: 'center',
        background: 'rgba(0, 0, 0, 0.65)',
        display: 'flex',
        inset: 0,
        justifyContent: 'center',
        position: 'fixed',
        zIndex: 10000,
      },
      onClick: (event: { target: unknown; currentTarget: unknown }) => {
        if (event.target === event.currentTarget) {
          (props.onCancel as (() => void) | undefined)?.();
        }
      },
    },
    createElement('div', { style: { background: '#1b2838', padding: '16px' } }, props.children),
  );
}

function ConfirmModal(props: Props): ReactNode {
  return createElement(
    ModalRoot,
    { onCancel: props.onCancel },
    props.strTitle ? createElement('h3', null, String(props.strTitle)) : null,
    props.strDescription ? createElement('p', null, String(props.strDescription)) : null,
    createElement(
      'div',
      { style: { display: 'flex', gap: '8px', justifyContent: 'flex-end' } },
      createElement(
        'button',
        { type: 'button', onClick: props.onCancel as (() => void) | undefined },
        String(props.strCancelButtonText ?? 'Cancel'),
      ),
      createElement(
        'button',
        { type: 'button', onClick: props.onOK as (() => void) | undefined },
        String(props.strOKButtonText ?? 'OK'),
      ),
    ),
  );
}

interface ErrorBoundaryState {
  error: Error | null;
}

class ErrorBoundary extends Component<{ children?: ReactNode }, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  render() {
    if (this.state.error) {
      return createElement(
        'div',
        { className: 'zerodeck-plugin-error' },
        `[ZeroDeck] plugin render failed: ${this.state.error.message}`,
      );
    }
    return this.props.children ?? null;
  }
}

function DiagnosticExport(props: Props & { exportName: string }): ReactNode {
  return createElement(
    'div',
    {
      className: 'zerodeck-unsupported-export',
      style: { border: '1px solid #b26a00', padding: '8px' },
    },
    `[ZeroDeck] @decky/ui export "${props.exportName}" is not implemented by this clean-room runtime.`,
    props.children,
  );
}

const dflBase: Record<string, unknown> = {
  ButtonItem,
  ConfirmModal,
  DialogButton,
  Dropdown,
  DialogButtonPrimary: DialogButton,
  DialogButtonSecondary: DialogButton,
  DropdownItem,
  ErrorBoundary,
  Field,
  Focusable,
  ModalRoot,
  PanelSection,
  PanelSectionRow,
  ProgressBar,
  SliderField,
  Spinner,
  SteamSpinner: Spinner,
  TextField,
  ToggleField,
  staticClasses: new Proxy({}, { get: (_target, key) => `zerodeck-${String(key)}` }),
  quickAccessMenuClasses: new Proxy({}, { get: (_target, key) => `zerodeck-qam-${String(key)}` }),
  vrDashboardClasses: new Proxy({}, { get: (_target, key) => `zerodeck-vr-${String(key)}` }),
  joinClassNames: (...names: unknown[]) => names.filter(Boolean).join(' '),
  sleep: (milliseconds: number) =>
    new Promise<void>((resolve) => setTimeout(resolve, milliseconds)),
  showModal,
  useQuickAccessVisible,
  findModule: () => undefined,
  findModuleByExport: () => undefined,
  findModuleExport: () => undefined,
  findModuleChild: () => undefined,
  findAllModules: () => [],
  findScope: () => undefined,
  findVar: () => undefined,
  createModuleMapping: () => new Map(),
  createScopeMapping: () => new Map(),
  createVarMapping: () => new Map(),
};

/**
 * Clean-room `window.DFL` surface for plugins built with `@decky/rollup`. Unknown PascalCase exports
 * resolve to a diagnostic component so one missing component cannot crash the whole menu.
 */
export const DFL: Record<string, unknown> = new Proxy(dflBase, {
  get(target, property) {
    if (Reflect.has(target, property)) return Reflect.get(target, property);
    if (typeof property === 'string' && /^[A-Z]/.test(property)) {
      return (props: Props) => createElement(DiagnosticExport, { ...props, exportName: property });
    }
    return undefined;
  },
});

export type ModalRef = { Close: () => void };

interface SteamReactDom {
  createRoot?: (container: Element) => { render: (node: ReactNode) => void; unmount: () => void };
}

/** Opens a modal using Steam's own ReactDOM, falling back with an explicit error. */
export function showModal(element: ReactNode): ModalRef {
  const reactDom = (globalThis as { SP_REACTDOM?: SteamReactDom }).SP_REACTDOM;
  if (!reactDom?.createRoot) {
    throw new Error('[ZeroDeck] Steam ReactDOM is not available for showModal');
  }
  const container = document.createElement('div');
  container.className = 'zerodeck-modal-host';
  document.body.appendChild(container);
  const root = reactDom.createRoot(container);
  root.render(element);
  const close = () => {
    root.unmount();
    container.remove();
  };
  return { Close: close };
}

/** The component type plugin bundles receive for `react` externalized to `SP_REACT`. */
export type DflComponent = ComponentType<Props>;
