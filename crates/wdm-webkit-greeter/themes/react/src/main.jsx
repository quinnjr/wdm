import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { config } from "@fortawesome/fontawesome-svg-core";
import "@fortawesome/fontawesome-svg-core/styles.css";
import { App } from "./App.jsx";
import "./input.css";

// Font Awesome injects its own <style> into <head> the first time an icon
// renders. Disabled here, with the stylesheet imported above instead, so the
// rules land in vendor/app.css and are in force before the first paint —
// otherwise every icon is briefly full-page-width while the CSS is missing,
// which on a login screen is a visible lurch rather than a subtlety.
//
// Set before anything renders, which is why it is here and not in App.jsx:
// the flag is read at first icon render, and a component module that set it
// would be racing its own import order.
config.autoAddCss = false;

// window.wdm is injected at document-start, before any of this runs, so it is
// already populated — there is no ready callback to wait for. Read once and
// passed down as a prop rather than reached for from inside components, which
// is what lets the whole tree be rendered against a stub.
const api = window.wdm;

const container = document.getElementById("root");

if (!api) {
  // Only reachable by opening index.html outside the greeter. Saying so beats
  // a blank page and a TypeError in a console nobody has open.
  container.textContent =
    "This is a wdm greeter theme. It has to be loaded by wdm-webkit-greeter, " +
    "which injects the window.wdm API it renders from.";
} else {
  // StrictMode double-invokes render and re-runs effects in development to
  // surface exactly the bugs this theme has to avoid — an effect that is not
  // idempotent, state derived during render. The build strips it; keeping it
  // here means the development run is the stricter one.
  createRoot(container).render(
    <StrictMode>
      <App api={api} />
    </StrictMode>,
  );
}
