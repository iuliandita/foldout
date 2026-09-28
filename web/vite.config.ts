import { defineConfig } from 'vite';

export default defineConfig({
  server: { proxy: { '/health': 'http://127.0.0.1:8787', '/api': 'http://127.0.0.1:8787' } },
  build: { sourcemap: false },
});
