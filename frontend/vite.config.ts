import { resolve } from 'node:path';
import { defineConfig, type Plugin } from 'vite';

const STEAM_GLOBAL_MODULES: Record<string, { globalKey: string; namedExports: string[] }> = {
  react: {
    globalKey: 'SP_REACT',
    namedExports: [
      'Children',
      'Component',
      'Fragment',
      'PureComponent',
      'StrictMode',
      'Suspense',
      'createContext',
      'createElement',
      'forwardRef',
      'lazy',
      'memo',
      'useCallback',
      'useContext',
      'useEffect',
      'useLayoutEffect',
      'useMemo',
      'useReducer',
      'useRef',
      'useState',
    ],
  },
  'react/jsx-runtime': {
    globalKey: 'SP_JSX',
    namedExports: ['Fragment', 'jsx', 'jsxs'],
  },
  'react-dom': {
    globalKey: 'SP_REACTDOM',
    namedExports: ['createPortal', 'createRoot', 'flushSync', 'hydrateRoot'],
  },
};

export function renderSteamGlobalModule(pkg: string, globalKey: string): string {
  const entry = STEAM_GLOBAL_MODULES[pkg];
  const named = entry ? entry.namedExports : [];
  const lines = [`const g = globalThis[${JSON.stringify(globalKey)}];`, 'export default g;'];
  for (const name of named) {
    lines.push(`export const ${name} = g?.${name};`);
  }
  return `${lines.join('\n')}\n`;
}

function steamExternalsPlugin(): Plugin {
  const prefix = '\0steam-global:';
  return {
    name: 'zerodeck-steam-globals',
    enforce: 'pre',
    resolveId(source) {
      if (source in STEAM_GLOBAL_MODULES) {
        return `${prefix}${source}`;
      }
      return null;
    },
    load(id) {
      if (!id.startsWith(prefix)) return null;
      const pkg = id.slice(prefix.length);
      const entry = STEAM_GLOBAL_MODULES[pkg];
      return entry ? renderSteamGlobalModule(pkg, entry.globalKey) : null;
    },
  };
}

export default defineConfig({
  plugins: [steamExternalsPlugin()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: true,
    target: 'es2024',
    rolldownOptions: {
      input: {
        index: resolve(import.meta.dirname, 'src/index.ts'),
        fallback: resolve(import.meta.dirname, 'src/fallback.ts'),
      },
      output: {
        format: 'es',
        entryFileNames: '[name].js',
        chunkFileNames: 'chunk-[hash].js',
      },
    },
  },
});
