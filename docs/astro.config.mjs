// @ts-check
import { defineConfig } from "astro/config";

// Project page, so everything is served under /wdm. Without `base` every
// absolute link and asset URL resolves to the user page at the domain root.
export default defineConfig({
  site: "https://quinnjr.github.io",
  base: "/wdm",
});
