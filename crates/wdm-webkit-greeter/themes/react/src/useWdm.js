import { useCallback, useLayoutEffect, useRef, useState } from "react";
import { initialState, reduce } from "./machine.js";

export const linkDead = (api) =>
  typeof api.link_dead !== "undefined" && api.link_dead;

// The bridge between wdm's imperative API and React's state, and the part of
// this theme that is actually about React.
//
// wdm calls three globals — show_prompt, show_message,
// authentication_complete — from outside React entirely, and may call them
// before, during or after any render. Four things about that are easy to get
// wrong, and each of them produces a login screen that looks fine and is not:
//
//  1. **The globals must exist before the first callback.** wdm can call them
//     as soon as the page's scripts have run. A callback landing while React
//     is still mounting is not queued anywhere — it throws inside the web
//     view, and the greeter counts that evaluation as failed and retransmits
//     it. They go in a layout effect, which runs before the browser paints.
//
//  2. **The state a callback decides from must be current, not captured.**
//     This is the stale-closure bug, and here it is not cosmetic: a
//     `show_prompt` closed over a render-old state would read a `buffered`
//     password that had already been spent and answer a second prompt with
//     it. The authoritative state therefore lives in a ref, and `send` reads
//     `ref.current` at the moment wdm calls it.
//
//  3. **Two callbacks can arrive in one evaluation.** wdm sends a batch of
//     statements — several `show_message` calls and then
//     `authentication_complete` — which run back to back before React has
//     re-rendered once. Deriving the next state from `useState`'s value would
//     make every call after the first read the same stale state. The ref is
//     updated synchronously, so each sees the previous one's result.
//
//  4. **Effects must not be performed during render.** `reduce` returns them
//     instead of calling `wdm.respond`, which is also what lets the whole
//     rule set be tested with no compositor on the other end — see
//     machine.test.js.

export const useWdm = (api) => {
  const [state, setState] = useState(() =>
    initialState({
      users: api.users,
      sessions: api.sessions,
      defaultSession: api.default_session,
    }),
  );

  // The authoritative copy. `state` is what React renders; this is what the
  // next event is computed from, and it is always at least as new.
  const current = useRef(state);

  const perform = useCallback(
    (effect) => {
      // Nothing reaches wdm once the link is dead: respond() and
      // start_session() post into a socket nobody reads. The reducer already
      // refuses to emit effects in that state; this is the second half of the
      // same rule, and it lives here because this is the side that knows the
      // call can throw.
      if (linkDead(api)) {
        return;
      }
      switch (effect.type) {
        case "authenticate":
          return api.authenticate(effect.username);
        case "respond":
          return api.respond(effect.answer);
        case "cancel":
          return api.cancel();
        case "startSession":
          return api.start_session(effect.sessionId);
        default:
          return undefined;
      }
    },
    [api],
  );

  // Stable for the life of the component, which is what lets the callbacks in
  // the layout effect below be installed once and never reinstalled.
  const send = useCallback(
    (event) => {
      const { state: next, effects } = reduce(current.current, event);
      current.current = next;
      setState(next);

      for (const effect of effects) {
        try {
          perform(effect);
        } catch (error) {
          // The API throws when called out of order, which is a bug in this
          // theme rather than something the user can act on. Letting it
          // propagate would unmount the tree and leave a blank login screen,
          // which is strictly worse than saying so on the screen.
          const text = error && error.message ? error.message : String(error);
          const failed = reduce(current.current, {
            type: "message",
            kind: "error",
            text: `Theme error: ${text}`,
          });
          current.current = failed.state;
          setState(failed.state);
        }
      }
    },
    [perform],
  );

  useLayoutEffect(() => {
    // Assigned to window, not added as listeners: these are the names wdm
    // evaluates by hand, and it checks for their existence before calling.
    window.show_prompt = (text, kind) =>
      send({ type: "prompt", text, secret: kind === "password" });

    window.show_message = (text, kind) =>
      send({ type: "message", text, kind: kind === "error" ? "error" : "info" });

    window.authentication_complete = () =>
      send({
        type: "complete",
        authenticated: api.is_authenticated,
        linkDead: linkDead(api),
      });

    // `link_dead` latches and has no callback of its own — it is a property
    // the bridge assigns. Polling is the only way to notice it while nothing
    // else is happening, which is exactly the case that matters: a greeter
    // sitting idle when wdm goes away must stop inviting a retry.
    const watch = window.setInterval(() => {
      if (linkDead(api) && current.current.phase !== "gaveUp") {
        send({ type: "linkDied" });
      }
    }, 500);

    return () => {
      window.clearInterval(watch);
      delete window.show_prompt;
      delete window.show_message;
      delete window.authentication_complete;
    };
  }, [api, send]);

  return { state, send };
};
