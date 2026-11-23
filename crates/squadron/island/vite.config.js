import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  plugins: [svelte(), tailwindcss()],
  build: {
    outDir: '../static',
    emptyOutDir: false,
    copyPublicDir: false,
    rollupOptions: {
      input: 'src/main.js',
      output: {
        format: 'es',
        entryFileNames: 'island.js',
        assetFileNames: (asset) =>
          asset.names?.[0]?.endsWith('.css') ? 'island.css' : 'assets/[name][extname]'
      }
    }
  }
});
