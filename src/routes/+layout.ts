// Tauri has no Node server: run as a client-only SPA (see svelte.config.js).
// Without this, the dev server renders pages once on the server, where
// browser APIs (requestAnimationFrame, WebGL, Tauri invoke) do not exist.
export const ssr = false;
