import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// React is bundled into app.js rather than loaded from a CDN — there is no
// network at a login screen — so its licence has to travel with it, as MIT
// requires. Emitted as a build artefact rather than copied afterwards because
// `emptyOutDir` wipes vendor/ on every build: a copy step outside the build
// is one that `npm run build` silently undoes, which is how CI came to be
// checking a vendor/ with the licences deleted.
const licences = () => ({
  name: "wdm-vendor-licences",
  generateBundle() {
    for (const [from, to] of [
      ["node_modules/react/LICENSE", "LICENSE-react.txt"],
      ["node_modules/tailwindcss/LICENSE", "LICENSE-tailwind.txt"],
    ]) {
      this.emitFile({
        type: "asset",
        fileName: to,
        source: readFileSync(from, "utf8"),
      });
    }
  },
});

// Vite's defaults assume a web server and a modern module graph. A wdm theme
// is neither: it is a directory of files opened as file:///…/index.html by a
// WebView whose content policy is `default-src file: data:`. Every option
// below is a departure from the defaults for that reason, and none of them is
// cosmetic — the first three each produce a blank login screen if dropped.
export default defineConfig(({ command }) => {
  // Set here rather than left to whoever runs the build, because
  // @vitejs/plugin-react chooses its JSX transform from this variable and not
  // from Vite's own `--mode`. Unset, it compiles every element to `jsxDEV`
  // from react/jsx-dev-runtime — whose production build exports
  // `jsxDEV: undefined` — and the bundle dies on its first element with
  // "jsxDEV is not a function". `vite build` reports success either way, and
  // the result is a login screen that is a blank white rectangle.
  //
  // Measured, not guessed: from a cold cache, `vite build` and
  // `vite build --mode production` both emitted 26 jsxDEV calls, and only
  // `NODE_ENV=production vite build` emitted none. A theme that builds
  // correctly only on a machine that happens to export NODE_ENV is a theme
  // that is broken for everyone else.
  //
  // Scoped to `build` so it does not leak into `vitest`, which sets its own.
  if (command === "build") {
    process.env.NODE_ENV = "production";
  }

  return {
  plugins: [react(), tailwindcss(), licences()],

  // Not optional, and not an optimisation. React's npm entry is CommonJS that
  // branches on `process.env.NODE_ENV` at *runtime*; in library mode Vite
  // leaves that expression for the consumer to define, which is correct for a
  // library and fatal here. There is no `process` on a file:// page, so the
  // first thing the bundle would do is throw ReferenceError — and the greeter
  // has no console anyone can open, so the symptom is a login screen that is
  // an empty div. Defining it also drops React's development build, which is
  // otherwise bundled alongside the production one.
  define: {
    "process.env.NODE_ENV": JSON.stringify("production"),
  },

  build: {
    // Built into vendor/, beside the hand-written index.html, so the theme has
    // the same shape as themes/arch: sources in src/, shipped artefacts in
    // vendor/, and an index.html a person can read.
    outDir: "vendor",
    emptyOutDir: true,

    // Debug-friendly output would be nice, but a sourcemap is a second file
    // whose absence is silent and whose presence ships the whole source into
    // the package. WDM_GREETER_DEBUG opens the inspector against the bundle.
    sourcemap: false,

    // Library mode, for a thing that is not a library — because it is the only
    // mode that emits the stylesheet as a *file*. With a plain JS entry Vite
    // inlines the CSS into the bundle and injects it with a <style> at
    // runtime, which means the page paints unstyled and then jumps once React
    // has run. On a login screen that flash is the first thing the user sees.
    //
    // `formats: ["iife"]` and not the default ESM. `<script type="module">` on
    // a file:// origin is fetched as a cross-origin request and blocked: the
    // page loads, the script never runs, and the login screen is an empty div
    // with no error anywhere the user can see it. This one option is the
    // difference between a working theme and a black rectangle.
    lib: {
      entry: "src/main.jsx",
      formats: ["iife"],
      // Required by Rollup for an IIFE even though nothing reads the global:
      // the bundle assigns its (empty) exports to it.
      name: "WdmReactTheme",
      // Fixed names, no content hash — index.html is hand-written and names
      // these, and a hash would rename them on every build.
      fileName: () => "app.js",
      cssFileName: "app",
    },

    // React is bundled rather than externalised: there is no CDN to load it
    // from and no import map on a file:// page. This is why the bundle is
    // ~400 kB, which is also the honest cost of the demonstration.
    rollupOptions: {
      output: { inlineDynamicImports: true },
    },
  },

  test: {
    // The reducer is pure — no DOM, no React, no window.wdm — so the default
    // node environment is the whole of what it needs. A jsdom environment
    // would be a dependency bought to run tests that never touch a document.
    environment: "node",
    include: ["src/**/*.test.js"],
  },
  };
});
