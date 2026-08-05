import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { JSDOM } from "jsdom";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

// The one test that runs the *shipped artefact* rather than the source.
//
// machine.test.js proves the rules are right. It cannot prove the theme
// renders: every failure that made this theme a blank white rectangle during
// development was in the build rather than the logic — a bundle that threw
// ReferenceError on `process` because Vite's library mode leaves
// `process.env.NODE_ENV` for the consumer to define, and a Tailwind build with
// no `@source` on index.html so the page's own classes were never generated.
//
// Neither is visible to a unit test, to `vite build` (both succeeded), or to
// the Rust drift checks. Both produce a login screen that is silently empty on
// the one screen in the system whose failures nobody can read a log from. So
// the bundle is mounted here, in a DOM, against a stub of window.wdm.

const here = dirname(fileURLToPath(import.meta.url));
const theme = join(here, "..");

const stubApi = () => ({
  users: [{ name: "ada", display_name: "Ada", last_session: "sway" }],
  sessions: [
    { id: "hyprland", name: "Hyprland" },
    { id: "sway", name: "Sway" },
  ],
  default_session: "hyprland",
  is_authenticated: false,
  in_authentication: false,
  authentication_user: null,
  link_dead: false,
  _prompt: null,
  calls: [],
  authenticate(username) {
    this.calls.push(["authenticate", username]);
  },
  respond(answer) {
    this.calls.push(["respond", answer]);
  },
  cancel() {
    this.calls.push(["cancel"]);
  },
  start_session(id) {
    this.calls.push(["start_session", id]);
  },
});

/**
 * Loads index.html and runs the built bundle in it, as the greeter does.
 *
 * Async because `createRoot().render()` does not mount inline: React 19
 * schedules the work and returns, so reading the DOM on the next line finds an
 * empty root whether the theme works or not. Waiting for the mount is what
 * makes the assertions below able to fail for the right reason.
 */
const mount = async (api) => {
  const html = readFileSync(join(theme, "index.html"), "utf8");
  const bundle = readFileSync(join(theme, "vendor/app.js"), "utf8");

  const dom = new JSDOM(html, {
    runScripts: "outside-only",
    pretendToBeVisual: true,
  });
  const errors = [];
  dom.window.addEventListener("error", (e) =>
    errors.push(String(e.error || e.message)),
  );
  // React reports a failed render through console.error rather than by
  // throwing, so a mount that fails would otherwise look like a mount that is
  // merely slow.
  dom.window.console.error = (...args) =>
    errors.push(`console.error: ${args.join(" ")}`);

  // window.wdm is injected at document-start by the greeter, so it is already
  // there when the bundle runs. Setting it afterwards would test a page the
  // greeter never serves.
  dom.window.wdm = api;

  try {
    dom.window.eval(bundle);
  } catch (error) {
    errors.push(String(error));
  }

  const root = dom.window.document.getElementById("root");
  // Bounded, so a theme that never mounts fails the assertions rather than
  // hanging the suite.
  for (let i = 0; i < 50 && root.children.length === 0 && errors.length === 0; i++) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  return { dom, doc: dom.window.document, errors };
};

describe("the built bundle", () => {
  let mounted;

  beforeAll(async () => {
    mounted = await mount(stubApi());
  });

  afterAll(() => {
    // useWdm polls link_dead on an interval, which keeps jsdom's timers — and
    // therefore the worker — alive after the assertions are done.
    mounted.dom.window.close();
  });

  it("runs without throwing", () => {
    // A bundle that throws leaves an empty <div id="root"> and no message.
    expect(mounted.errors).toEqual([]);
  });

  it("renders the form rather than an empty root", () => {
    const root = mounted.doc.getElementById("root");
    expect(root).not.toBeNull();
    expect(root.children.length).toBeGreaterThan(0);
    expect(mounted.doc.querySelector("form")).not.toBeNull();
  });

  it("populates the user and session dropdowns from the API", () => {
    const selects = mounted.doc.querySelectorAll("select");
    expect(selects).toHaveLength(2);
    expect([...selects[0].options].map((o) => o.value)).toEqual(["ada"]);
    expect([...selects[1].options].map((o) => o.value)).toEqual([
      "hyprland",
      "sway",
    ]);
    // Preselected from the user's history, not left on the first entry.
    expect(selects[1].value).toBe("sway");
  });

  it("masks the answer field", () => {
    expect(mounted.doc.querySelector("input").type).toBe("password");
  });

  it("renders the Font Awesome icons inline", () => {
    // Icons are SVG components, so they are in the DOM rather than in a font
    // file — which is the reason this theme installs no webfont. A tree-shake
    // that dropped them, or an icon imported by string name against a library
    // that was never registered, both render nothing and throw nothing.
    const icons = mounted.doc.querySelectorAll("svg[data-icon]");
    expect(icons.length).toBeGreaterThanOrEqual(4);
    const names = [...icons].map((svg) => svg.getAttribute("data-icon"));
    expect(names).toContain("user");
    expect(names).toContain("display");
    expect(names).toContain("right-to-bracket");
    // The prompt icon tracks whether the answer is masked; at rest it is.
    expect(names).toContain("lock");
    // Every path must have actual geometry — an icon whose data failed to
    // bundle still renders an <svg> element, just an empty one.
    for (const path of mounted.doc.querySelectorAll("svg[data-icon] path")) {
      expect(path.getAttribute("d")?.length ?? 0).toBeGreaterThan(10);
    }
  });

  it("does not inject Font Awesome's CSS at runtime", () => {
    // autoAddCss is disabled and the stylesheet imported into the bundle
    // instead, so the rules are in force before the first paint. If that
    // regressed, the icons would each be full-page-width for a frame.
    const injected = [...mounted.doc.querySelectorAll("style")].some((s) =>
      (s.textContent || "").includes("svg-inline--fa"),
    );
    expect(injected).toBe(false);
  });

  it("installs the three callbacks the greeter calls by name", () => {
    // These are globals wdm evaluates by hand. If the layout effect that
    // assigns them ever moves to a plain effect, or the component fails to
    // mount, the greeter's prompts go nowhere and the screen just waits.
    for (const name of [
      "show_prompt",
      "show_message",
      "authentication_complete",
    ]) {
      expect(typeof mounted.dom.window[name]).toBe("function");
    }
  });

  it("does not open a PAM conversation at load", () => {
    // The lockout rule, checked against the running artefact rather than the
    // source: an unattended login screen must not spend a login attempt.
    expect(mounted.dom.window.wdm.calls).toEqual([]);
  });
});

describe("the built stylesheet", () => {
  const css = () => readFileSync(join(theme, "vendor/app.css"), "utf8");

  it("contains the utilities index.html uses on <body>", () => {
    // Tailwind emits only what it can prove is used, and index.html is not a
    // file it scans unless `@source` names it. Without this the page renders
    // with an unstyled body — white, unaligned — while every class inside the
    // React tree works, which is a confusing way to be broken.
    const sheet = css();
    for (const utility of ["min-h-screen", "items-center", "justify-center"]) {
      expect(sheet).toContain(utility);
    }
  });

  it("contains the theme colour index.html names", () => {
    // The utility, not just the custom property. `@theme` emits the variable
    // whether or not anything uses it, so asserting on `--color-wdm-night`
    // alone would pass even with the class ungenerated and the page white.
    expect(css()).toContain("bg-wdm-night");
  });

  it("contains utilities used only inside the React tree", () => {
    expect(css()).toContain("rounded-3xl");
  });

  it("contains Font Awesome's own rules", () => {
    // Imported rather than injected at runtime. Without them every icon is
    // sized by the raw SVG's intrinsic dimensions — which is the width of the
    // card.
    expect(css()).toContain("svg-inline--fa");
  });
});
