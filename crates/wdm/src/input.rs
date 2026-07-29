//! Input routing.
//!
//! There is exactly one focusable surface, so this is far simpler than a general
//! compositor's input handling: events go to the greeter, and the only key wdm
//! intercepts is the VT switch chord.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, KeyState,
    KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchEvent,
};
use smithay::input::keyboard::{FilterResult, Keysym, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent,
};
use smithay::input::touch::{DownEvent, MotionEvent as TouchMotionEvent, UpEvent};
use smithay::utils::SERIAL_COUNTER;

use crate::comp::Wdm;

/// A VT switch request the compositor must act on.
///
/// Returned rather than performed because only the udev backend has a seat to
/// switch on; the nested backend logs and ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchVt(pub i32);

/// Route one input event to the greeter.
///
/// Returns a VT switch request when the user pressed one. Ctrl+Alt+F1 must
/// always work: it is the documented way back into a machine whose greeter has
/// wedged, and the greeter must never be able to swallow it.
pub fn handle<B: InputBackend>(state: &mut Wdm, event: InputEvent<B>) -> Option<SwitchVt> {
    match event {
        InputEvent::Keyboard { event } => return keyboard::<B>(state, event),

        InputEvent::PointerMotion { event } => {
            let pointer = state.seat.get_pointer()?;
            let serial = SERIAL_COUNTER.next_serial();

            // With a single fullscreen surface the pointer position is simply
            // clamped to the output, so relative motion is accumulated onto the
            // last known location.
            let location = pointer.current_location() + event.delta();
            let location = clamp_to_output(state, location);
            let focus = focus_under(state);

            pointer.motion(
                state,
                focus.clone(),
                &MotionEvent {
                    location,
                    serial,
                    time: event.time_msec(),
                },
            );
            pointer.relative_motion(
                state,
                focus,
                &RelativeMotionEvent {
                    delta: event.delta(),
                    delta_unaccel: event.delta_unaccel(),
                    utime: event.time(),
                },
            );
            pointer.frame(state);
        }

        InputEvent::PointerMotionAbsolute { event } => {
            let pointer = state.seat.get_pointer()?;
            let size = output_size(state)?;

            let location = event.position_transformed(size);
            let focus = focus_under(state);

            pointer.motion(
                state,
                focus,
                &MotionEvent {
                    location,
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                },
            );
            pointer.frame(state);
        }

        InputEvent::PointerButton { event } => {
            let pointer = state.seat.get_pointer()?;
            pointer.button(
                state,
                &ButtonEvent {
                    button: event.button_code(),
                    state: if event.state() == ButtonState::Pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                },
            );
            pointer.frame(state);
        }

        InputEvent::PointerAxis { event } => {
            let pointer = state.seat.get_pointer()?;

            let source = event.source();
            let mut frame = AxisFrame::new(event.time_msec()).source(source);

            for axis in [Axis::Horizontal, Axis::Vertical] {
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.v120(axis, discrete as i32);
                }
                if let Some(amount) = event.amount(axis) {
                    frame = frame.value(axis, amount);
                } else if let Some(discrete) = event.amount_v120(axis) {
                    // Some devices report only discrete steps; synthesise a
                    // continuous value so clients that ignore v120 still scroll.
                    frame = frame.value(axis, discrete / 120.0 * 15.0);
                }
            }

            if source == AxisSource::Finger {
                if event.amount(Axis::Horizontal) == Some(0.0) {
                    frame = frame.stop(Axis::Horizontal);
                }
                if event.amount(Axis::Vertical) == Some(0.0) {
                    frame = frame.stop(Axis::Vertical);
                }
            }

            pointer.axis(state, frame);
            pointer.frame(state);
        }

        InputEvent::TouchDown { event } => {
            let touch = state.seat.get_touch()?;
            let size = output_size(state)?;
            let focus = focus_under(state);

            touch.down(
                state,
                focus,
                &DownEvent {
                    slot: event.slot(),
                    location: event.position_transformed(size),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                },
            );
            touch.frame(state);
        }

        InputEvent::TouchMotion { event } => {
            let touch = state.seat.get_touch()?;
            let size = output_size(state)?;

            touch.motion(
                state,
                focus_under(state),
                &TouchMotionEvent {
                    slot: event.slot(),
                    location: event.position_transformed(size),
                    time: event.time_msec(),
                },
            );
            touch.frame(state);
        }

        InputEvent::TouchUp { event } => {
            let touch = state.seat.get_touch()?;
            touch.up(
                state,
                &UpEvent {
                    slot: event.slot(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time: event.time_msec(),
                },
            );
            touch.frame(state);
        }

        InputEvent::TouchCancel { .. } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.cancel(state);
                touch.frame(state);
            }
        }

        InputEvent::TouchFrame { .. } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.frame(state);
            }
        }

        // Device add and remove are handled by the backend, which owns the
        // libinput context.
        _ => {}
    }

    None
}

fn keyboard<B: InputBackend>(state: &mut Wdm, event: B::KeyboardKeyEvent) -> Option<SwitchVt> {
    let keyboard = state.keyboard.clone();
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();
    let key_state = event.state();

    keyboard.input(
        state,
        event.key_code(),
        key_state,
        serial,
        time,
        |_, modifiers, handle| {
            if key_state != KeyState::Pressed {
                return FilterResult::Forward;
            }
            match vt_for(modifiers, handle.modified_sym()) {
                // Intercepted, never forwarded: a greeter that could see this
                // chord could also choose to swallow it, and then a wedged
                // greeter would have no escape hatch.
                Some(vt) => FilterResult::Intercept(SwitchVt(vt)),
                None => FilterResult::Forward,
            }
        },
    )
}

/// Map Ctrl+Alt+F1..F12 to a VT number.
///
/// Both Ctrl and Alt are required, matching the kernel's own VT chord, so a
/// greeter that wants a plain F-key still gets it.
fn vt_for(modifiers: &ModifiersState, keysym: Keysym) -> Option<i32> {
    if !(modifiers.ctrl && modifiers.alt) {
        return None;
    }

    let raw = keysym.raw();
    // XF86Switch_VT_1..12 are what xkb emits for the chord when the layout has
    // the VT actions bound; F1..F12 cover the layouts that do not.
    const XF86_SWITCH_VT_1: u32 = 0x1008fe01;
    const F1: u32 = 0xffbe;

    if (XF86_SWITCH_VT_1..XF86_SWITCH_VT_1 + 12).contains(&raw) {
        return Some((raw - XF86_SWITCH_VT_1 + 1) as i32);
    }
    if (F1..F1 + 12).contains(&raw) {
        return Some((raw - F1 + 1) as i32);
    }

    None
}

/// The surface pointer and touch events should go to.
fn focus_under(state: &Wdm) -> Option<(smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, smithay::utils::Point<f64, smithay::utils::Logical>)> {
    let primary = state.outputs.first();
    state
        .layers
        .iter()
        .find(|l| primary.is_some() && l.output.as_ref() == primary)
        .or_else(|| state.layers.last())
        .map(|l| (l.surface.wl_surface().clone(), (0.0, 0.0).into()))
}

/// The primary output's logical size.
///
/// Integral because `position_transformed` works in whole logical pixels; the
/// fractional scale is applied when computing it.
fn output_size(state: &Wdm) -> Option<smithay::utils::Size<i32, smithay::utils::Logical>> {
    let output = state.outputs.first()?;
    let mode = output.current_mode()?;
    let scale = output.current_scale().fractional_scale();
    Some(
        (
            (f64::from(mode.size.w) / scale).round() as i32,
            (f64::from(mode.size.h) / scale).round() as i32,
        )
            .into(),
    )
}

fn clamp_to_output(
    state: &Wdm,
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let Some(size) = output_size(state) else {
        return location;
    };
    // A pointer allowed off the edge would be invisible and unrecoverable, since
    // there is no other surface to move it back onto.
    (
        location.x.clamp(0.0, f64::from(size.w) - 1.0),
        location.y.clamp(0.0, f64::from(size.h) - 1.0),
    )
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(ctrl: bool, alt: bool) -> ModifiersState {
        ModifiersState {
            ctrl,
            alt,
            ..ModifiersState::default()
        }
    }

    #[test]
    fn ctrl_alt_f1_switches_to_vt1() {
        assert_eq!(
            vt_for(&modifiers(true, true), Keysym::from(0xffbe)),
            Some(1)
        );
    }

    #[test]
    fn ctrl_alt_f7_switches_to_vt7() {
        // wdm's own VT: switching back to it must work too.
        assert_eq!(
            vt_for(&modifiers(true, true), Keysym::from(0xffbe + 6)),
            Some(7)
        );
    }

    #[test]
    fn recognises_xf86_switch_vt_keysyms() {
        assert_eq!(
            vt_for(&modifiers(true, true), Keysym::from(0x1008fe01)),
            Some(1)
        );
        assert_eq!(
            vt_for(&modifiers(true, true), Keysym::from(0x1008fe01 + 11)),
            Some(12)
        );
    }

    #[test]
    fn requires_both_modifiers() {
        // A greeter that wants plain F1, or Alt+F1, still receives it.
        let f1 = Keysym::from(0xffbe);
        assert_eq!(vt_for(&modifiers(false, false), f1), None);
        assert_eq!(vt_for(&modifiers(true, false), f1), None);
        assert_eq!(vt_for(&modifiers(false, true), f1), None);
    }

    #[test]
    fn other_keys_are_not_vt_switches() {
        // 'a', and F13 which is past the range.
        assert_eq!(vt_for(&modifiers(true, true), Keysym::from(0x061)), None);
        assert_eq!(
            vt_for(&modifiers(true, true), Keysym::from(0xffbe + 12)),
            None
        );
        assert_eq!(
            vt_for(&modifiers(true, true), Keysym::from(0x1008fe01 + 12)),
            None
        );
    }
}
