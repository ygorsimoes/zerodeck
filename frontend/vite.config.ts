import { resolve } from 'node:path';
import { defineConfig, type Plugin } from 'vite';

const STEAM_GLOBAL_MODULES: Record<string, string> = {
  react: 'SP_REACT',
  'react/jsx-runtime': 'SP_JSX',
  'react-dom': 'SP_REACTDOM',
};

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
      const globalKey = STEAM_GLOBAL_MODULES[pkg];
      return `const g = globalThis[${JSON.stringify(globalKey)}];\nexport default g;\n`;
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
