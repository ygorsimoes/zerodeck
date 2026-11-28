# Plugin Frontend ABI (Decky 3.x, `@decky/ui` 5.x)

This document records the public browser contract ZeroDeck implements for Decky plugin frontends.
It is derived from published plugin bundles and the public `@decky/api` / `@decky/rollup`
documentation, not from copied loader code.

## Baseline

- Decky Loader 3.x, `@decky/api` 1.x with API version 2, `@decky/ui` 5.x.
- Plugin bundles are built by `@decky/rollup` with Rollup, TypeScript and Vite-compatible ESM output.
- `@decky/api` and `@decky/manifest` are bundled into the plugin. The loader must provide the
  globals below before a plugin module is evaluated.
- ZeroDeck implements the API clean-room in `frontend/src/decky/`. It does not bundle `@decky/ui`,
  `@decky/api` or any other LGPL-2.1 code.

## Globals

| Global | Contract |
|---|---|
| `SP_REACT` | Steam's React instance. `react` imports in the plugin are rewritten to this global. |
| `SP_JSX` | Steam's JSX runtime (`jsx`, `jsxs`, `Fragment`). |
| `SP_REACTDOM` | Steam's ReactDOM (`createPortal`, `createRoot`, `flushSync`). |
| `DFL` | Compatibility surface for `@decky/ui` imports. See coverage below. |
| `DeckyBackend` | `WSRouter`-compatible client (`call`, `callable`, event listeners). |
| `__DECKY_SECRET_INTERNALS_DO_NOT_USE_OR_YOU_WILL_BE_FIRED_deckyLoaderAPIInit` | `connect(version, pluginName)` hook read by `@decky/api` at module evaluation. |
| `DeckyPluginLoader` | Loader instance exposing `plugins()` for the Quick Access Menu. |

## Bundle loading

- ESM: `GET /plugins/<manifest.name>/dist/index.js?generation=<version>-<load_type>-<reload>`.
  The module's default export is called as a factory; its return value is the `Plugin` object.
- Legacy IIFE: `GET /plugins/<manifest.name>/frontend_bundle` with `X-Decky-Auth`. The bundle is
  evaluated as an expression and the resulting function receives the legacy API object.
- Assets: `GET /plugins/<manifest.name>/dist/<file>` and the Rollup public path
  `GET /plugins/<manifest.name>/<file>` (resolved inside `dist/` then `assets/`). Both are public
  GET routes behind the plugin `SafePath` jail; `data/` stays public for Decky parity.
- The generation segment changes on plugin version, load type and explicit debug reload, so a
  replaced bundle is never served from the dynamic-import cache.
- A plugin failure is isolated: the loader records it, keeps other plugins loaded, and exposes the
  error through `DeckyPluginLoader.loadErrors()`.

## `@decky/api` version 2

`connect(version, pluginName)` returns an object with:

- `call(route, ...args)` and `callable(route)` -> `loader/call_plugin_method` with
  `[pluginName, route, ...args]`.
- `addEventListener(event, listener)` / `removeEventListener`: filtered from
  `loader/plugin_event` by plugin and event name.
- `executeInTab`, `injectCssIntoTab`, `removeCssFromTab`: current-page execution/style helpers.
- `fetchNoCors` and `getExternalResourceURL`: authenticated `/fetch` proxy with `X-Decky-Header-*`
  translation.
- `useQuickAccessVisible`, `routerHook`, `toaster`.
- `openFilePicker` is declared but currently rejects with `UnsupportedFeatureError`; it is tracked
  for the Quick Access Menu milestone.

Legacy bundles receive the matching `callPluginMethod` -> `loader/call_legacy_plugin_method`,
`callServerMethod`, `fetchNoCors`, `routerHook`, `toaster` and tab-style helpers.

`routerHook` routes and patches are stored by the compatibility layer but are not applied to
Steam's internal router. Plugins that require route patches must be reported by the strict harness.

## `@decky/ui` (`DFL`) coverage

The clean-room subset currently covers: `PanelSection`, `PanelSectionRow`, `ButtonItem`,
`ToggleField`, `SliderField`, `TextField`, `Dropdown`, `DropdownItem`, `DialogButton`,
`DialogButtonPrimary`, `DialogButtonSecondary`, `Field`, `Focusable`, `ProgressBar`, `Spinner`,
`SteamSpinner`, `ModalRoot`, `ConfirmModal`, `ErrorBoundary`, `showModal`, `staticClasses`,
`quickAccessMenuClasses`, `vrDashboardClasses`, `joinClassNames`, `sleep`,
`useQuickAccessVisible`, and no-op module/scope search helpers.

Any unknown PascalCase export resolves to a component that renders an explicit
`@decky/ui export "<name>" is not implemented` diagnostic instead of throwing during module
evaluation. This is a **functional** guarantee (the plugin and menu stay alive), not a promise of
visual parity.

## Module system, `initModules` and scope resolution

The official `@decky/ui` module system (`initModules`, `findModuleByExport`, V8 `[[Scopes]]`
resolution through CDP) is LGPL-2.1 code and is **not redistributed** with ZeroDeck. The clean-room
`DFL` subset does not import Steam's internal modules, so it does not call `initModules` or
`instrumentation/resolve_scope`.

Consequences:

- Plugins that only use the component subset and `@decky/api` load without recompilation.
- Plugins that call `findModuleByExport`, `findScope` or depend on Steam internal components will
  receive the diagnostic path and must be listed in the strict harness report.
- If the official `@decky/ui` runtime is adopted later, it must be shipped unmodified as a separate
  dynamically loaded module with its LGPL-2.1 notice, corresponding source and relinking
  information. It must not be copied into the MIT source tree. The reserved backend route name for
  that integration is `instrumentation/resolve_scope`.

## Harness profiles

- **Functional:** load ESM, legacy and frontend-only fixtures; exercise `@decky/api` calls, events,
  asset URLs, unload and reload. Fixtures use the same external globals as `@decky/rollup` output.
- **Strict:** fail on missing `DFL` exports, duplicate loads, leaked event listeners, unhandled
  plugin errors, generation reuse, or a plugin that prevents its neighbors from loading.

The current Node tests implement both profiles with injectable module/text loaders. Official
`@decky/rollup` and `@decky/ui` artifacts remain development-only test inputs; they are never
included in a ZeroDeck release archive.
