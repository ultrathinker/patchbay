//! The optional popover window (S13, `_planning/WINDOW_UI_PLAN.md` W-D2/W-D6/W1).
//!
//! A frameless, always-on-top, taskbar-less webview anchored to the tray icon —
//! the same mental object as the Windows volume flyout, not an application
//! window. It is a second VIEW over the same [`crate::app_state::AppState`]; all
//! business logic stays in `app_state.rs` (W-D4).
//!
//! ## Lifecycle (W-D6)
//! Created LAZILY on first use and then HIDDEN rather than closed. A user who
//! never leaves `ui_mode = "tray"` never pays for a WebView2 process, and
//! startup is untouched; after the first open, showing it again is instant.
//!
//! ## Closing (S13.2) — unlike a flyout, NOT on losing focus
//! A first version hid the popover the moment it lost focus, the ordinary
//! flyout behaviour. That made resizing it impossible: grabbing the (invisible,
//! `decorations(false)`) resize border to drag an edge is itself a click
//! outside the webview's content, so the window closed under the user's hand
//! before a drag could start. There are now exactly two ways to close it — the
//! tray icon (see [`toggle_popover`]) and the header's ✕ button
//! (`ui_close_window`) — and neither the window losing focus nor a click
//! landing anywhere else does anything to it.
//!
//! ## Never trap the user (W-D7)
//! If the webview cannot be created at all (no WebView2 runtime, GPU fault),
//! the failure is logged, remembered in [`PopoverState::creation_failed`], and
//! the tray falls back to the native menu FOR THIS SESSION ONLY — the config
//! file is not rewritten, so a transient failure does not silently change the
//! user's setting.

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::utils::log::log;

/// Window label the popover is addressed by.
pub const POPOVER_LABEL: &str = "popover";

/// Popover size in LOGICAL pixels — the size it opens at on first creation,
/// and (S13.1) the SMALLEST it can be shrunk to.
///
/// Not content-driven (W1 / review finding 10): a popover pinned above a
/// bottom taskbar must keep its bottom edge stable while open, and a height
/// that changed with the content would push the window down through the
/// taskbar. Lists scroll internally instead (`.list { overflow-y: auto }`,
/// `app.css`) — the SAME mechanism that now also makes user-resizing
/// pleasant: grow the window and the scrollbar recedes on its own, with no
/// code on either side needing to know about the other.
///
/// (S13.1) The window is resizable — a fixed size read fine on paper but the
/// footer's six links (`Jacks · Agents N · Settings · Logs · Reload · Quit`)
/// wrapped onto a second line at 400 px wide the moment an agent count grew a
/// digit. `POPOVER_W` is now also the hard MINIMUM width: it is sized with
/// margin above what that row needs, so shrinking the window can never bring
/// the wrap back. There is no maximum — [`popover_origin`]'s existing
/// work-area clamping (already covering a window larger than the screen, see
/// its tests) is what keeps an enormous window on-screen.
pub const POPOVER_W: f64 = 460.0;
pub const POPOVER_H: f64 = 560.0;

/// (S13.1) Minimum HEIGHT the user can shrink the popover to — enough for the
/// header, the servers toolbar and about three server rows before the list
/// has to start scrolling. Deliberately smaller than [`POPOVER_H`]: unlike
/// width, a short popover is a legitimate choice (glance at a couple of
/// servers), not a broken one, so height has real room to shrink.
pub const MIN_POPOVER_H: f64 = 320.0;

/// Gap between the popover and the screen edge it is anchored to, in physical
/// pixels at 100% scaling. Kept small: a flyout should look attached to the
/// tray, not floating near it.
const EDGE_GAP: i32 = 8;

/// Runtime state for the popover, kept in Tauri's managed state.
#[derive(Default)]
pub struct PopoverState {
    /// The tray icon's screen rectangle from the most recent tray event, used
    /// to position the popover against it.
    pub tray_rect: Mutex<Option<RectPx>>,

    /// Whether creating the webview has already failed once. Prevents retrying
    /// (and re-logging) on every click, and drives the session-only fallback to
    /// the native menu.
    pub creation_failed: AtomicBool,
}

/// A screen rectangle in physical pixels — the common currency between a tray
/// event's `Rect`, a monitor's work area, and [`popover_origin`].
///
/// A plain data type on purpose: the placement maths is the part most likely to
/// be wrong on a real desktop (taskbar on any edge, auto-hidden, two monitors at
/// different scaling), and it must be unit-testable without a window, a monitor
/// or a running Tauri app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectPx {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl RectPx {
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn center_x(&self) -> i32 {
        self.x + self.w / 2
    }
    pub fn center_y(&self) -> i32 {
        self.y + self.h / 2
    }
}

/// Which screen edge the taskbar (and therefore the notification area) is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskbarEdge {
    Bottom,
    Top,
    Left,
    Right,
}

/// Infer the taskbar edge from where the tray icon sits relative to the
/// monitor's WORK AREA (the screen minus the taskbar).
///
/// Normally the tray is outside the work area — that gap IS the taskbar — so
/// the edge follows from which side it lies on. When the taskbar is
/// **auto-hidden** the work area covers the whole screen and the tray rect
/// falls INSIDE it, so no edge can be read off directly; then we fall back to
/// whichever edge the tray icon is closest to, which is the same answer for
/// every real desktop layout.
pub fn taskbar_edge(tray: RectPx, work: RectPx) -> TaskbarEdge {
    if tray.y >= work.bottom() {
        return TaskbarEdge::Bottom;
    }
    if tray.bottom() <= work.y {
        return TaskbarEdge::Top;
    }
    if tray.x >= work.right() {
        return TaskbarEdge::Right;
    }
    if tray.right() <= work.x {
        return TaskbarEdge::Left;
    }

    // Auto-hidden (or an overlapping/unknown layout): nearest edge wins.
    let d_bottom = (work.bottom() - tray.center_y()).max(0);
    let d_top = (tray.center_y() - work.y).max(0);
    let d_right = (work.right() - tray.center_x()).max(0);
    let d_left = (tray.center_x() - work.x).max(0);

    let min = d_bottom.min(d_top).min(d_right).min(d_left);
    if min == d_bottom {
        TaskbarEdge::Bottom
    } else if min == d_top {
        TaskbarEdge::Top
    } else if min == d_right {
        TaskbarEdge::Right
    } else {
        TaskbarEdge::Left
    }
}

/// Top-left corner for the popover, in physical screen pixels.
///
/// Anchors to the taskbar edge, aligns to the tray icon along the other axis,
/// and clamps the whole window inside the work area so it can never spill onto
/// the taskbar or off the monitor. All coordinates are virtual-desktop
/// coordinates, which are NEGATIVE on a monitor left of or above the primary —
/// hence i32 throughout and clamping against the work area's own origin rather
/// than against zero.
pub fn popover_origin(tray: RectPx, work: RectPx, win_w: i32, win_h: i32) -> (i32, i32) {
    let (mut x, mut y) = match taskbar_edge(tray, work) {
        TaskbarEdge::Bottom => (
            tray.center_x() - win_w / 2,
            work.bottom() - win_h - EDGE_GAP,
        ),
        TaskbarEdge::Top => (tray.center_x() - win_w / 2, work.y + EDGE_GAP),
        TaskbarEdge::Right => (
            work.right() - win_w - EDGE_GAP,
            tray.center_y() - win_h / 2,
        ),
        TaskbarEdge::Left => (work.x + EDGE_GAP, tray.center_y() - win_h / 2),
    };

    // Clamp inside the work area. `max` is applied LAST so that on a work area
    // smaller than the window (a tiny screen, or a huge scaling factor) the
    // window's top-left stays visible instead of being pushed off the far edge.
    x = x.min(work.right() - win_w - EDGE_GAP).max(work.x + EDGE_GAP);
    y = y.min(work.bottom() - win_h - EDGE_GAP).max(work.y + EDGE_GAP);

    (x, y)
}

/// Create the popover if it does not exist yet, returning it either way.
///
/// The window is built HIDDEN and unfocused so a creation triggered by
/// something other than a user click cannot steal focus mid-typing;
/// [`show_popover`] is what makes it visible.
fn ensure_popover(app: &AppHandle) -> Result<tauri::WebviewWindow, tauri::Error> {
    if let Some(w) = app.get_webview_window(POPOVER_LABEL) {
        return Ok(w);
    }

    log("popover: creating webview window (first use)");
    let window = WebviewWindowBuilder::new(app, POPOVER_LABEL, WebviewUrl::App("index.html".into()))
        .title("Patchbay")
        .inner_size(POPOVER_W, POPOVER_H)
        // (S13.1) User-resizable, with a floor: below `POPOVER_W` the footer
        // wraps (see the constant's doc comment), so width cannot shrink past
        // it at all. Height has genuine room — down to `MIN_POPOVER_H`. No
        // upper bound on either axis; `popover_origin`'s work-area clamp is
        // what keeps a since-enlarged window from drifting off-screen.
        .resizable(true)
        .min_inner_size(POPOVER_W, MIN_POPOVER_H)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false)
        .build()?;

    Ok(window)
}

/// (S13 §4.7) Step out of the way of a blocking Win32 dialog.
///
/// The first-connection approval prompt is a `MessageBoxW`. The popover is
/// always-on-top, so a dialog raised while it is open can be painted BEHIND it
/// — and since the dialog is modal to its own thread, the result looks exactly
/// like a frozen machine: a window that will not respond and a dialog the user
/// cannot see or reach.
///
/// Called around any blocking native dialog. Idempotent and safe when no window
/// exists (the common case: `ui_mode = "tray"`).
pub fn set_yielding(app: &AppHandle, yielding: bool) {
    if let Some(w) = app.get_webview_window(POPOVER_LABEL) {
        if yielding {
            // Hiding, not merely un-topmosting: an approval prompt is a
            // question about a NEW agent, and the popover's own contents are
            // about to be invalidated by the answer anyway.
            if let Err(e) = w.set_always_on_top(false) {
                log(&format!("popover: clearing always-on-top failed: {}", e));
            }
            if let Err(e) = w.hide() {
                log(&format!("popover: hide-for-dialog failed: {}", e));
            }
        } else if let Err(e) = w.set_always_on_top(true) {
            log(&format!("popover: restoring always-on-top failed: {}", e));
        }
    }
}

/// Hide the popover if it exists. Never destroys it (W-D6).
pub fn hide_popover(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(POPOVER_LABEL) {
        if let Err(e) = w.hide() {
            log(&format!("popover: hide failed: {}", e));
        }
    }
}

/// Position and show the popover against the tray icon.
///
/// Returns `false` if the window could not be created — the caller then falls
/// back to the native menu for this session (W-D7).
pub fn show_popover(app: &AppHandle, tray: RectPx) -> bool {
    let window = match ensure_popover(app) {
        Ok(w) => w,
        Err(e) => {
            log(&format!(
                "popover: FAILED to create webview window ({}); falling back to the tray menu for this session",
                e
            ));
            if let Some(state) = app.try_state::<PopoverState>() {
                state.creation_failed.store(true, Ordering::SeqCst);
            }
            return false;
        }
    };

    // The monitor under the TRAY ICON, not under the window's last position:
    // with two monitors at different scaling the icon is the anchor that
    // matters, and the window may still be sitting on the other screen.
    let monitor = app
        .monitor_from_point(tray.center_x() as f64, tray.center_y() as f64)
        .ok()
        .flatten();

    // (S13.1) The window's ACTUAL current size, not the `POPOVER_W`/`POPOVER_H`
    // defaults: once resizing is allowed, those two can disagree, and
    // positioning off the wrong one drifts the popover away from the tray
    // icon by exactly the size difference — worse the more the user resized
    // it. `inner_size()` already reports physical pixels, so no separate
    // scale-factor multiplication is needed for it (unlike the constants,
    // which are logical and only get one in the fallback arms below).
    let actual_size = window.inner_size().ok();

    let (win_w, win_h, work) = match &monitor {
        Some(m) => {
            let wa = m.work_area();
            let work = RectPx {
                x: wa.position.x,
                y: wa.position.y,
                w: wa.size.width as i32,
                h: wa.size.height as i32,
            };
            match actual_size {
                Some(sz) => (sz.width as i32, sz.height as i32, work),
                None => {
                    // Reading it back failed (should not happen for a window
                    // just created/shown before) — fall back to the default,
                    // scaled for this monitor.
                    let scale = m.scale_factor();
                    log("popover: could not read the current window size, positioning at the default size");
                    (
                        (POPOVER_W * scale).round() as i32,
                        (POPOVER_H * scale).round() as i32,
                        work,
                    )
                }
            }
        }
        None => {
            // No monitor info (rare; e.g. the icon's monitor was just
            // unplugged). Anchor to the tray rect itself so the window still
            // appears somewhere sane rather than at (0,0).
            log("popover: no monitor for the tray point, positioning against the tray rect");
            (
                POPOVER_W as i32,
                POPOVER_H as i32,
                RectPx {
                    x: tray.x - POPOVER_W as i32,
                    y: tray.y - POPOVER_H as i32,
                    w: POPOVER_W as i32 * 2,
                    h: POPOVER_H as i32 * 2,
                },
            )
        }
    };

    let (x, y) = popover_origin(tray, work, win_w, win_h);
    if let Err(e) = window.set_position(tauri::PhysicalPosition { x, y }) {
        log(&format!("popover: set_position failed: {}", e));
    }
    if let Err(e) = window.show() {
        log(&format!("popover: show failed: {}", e));
        return false;
    }
    // Focus AFTER showing: the popover is a focus trap for the keyboard map
    // (plan §4.3), and an unfocused always-on-top window would swallow clicks
    // without ever receiving a key.
    if let Err(e) = window.set_focus() {
        log(&format!("popover: set_focus failed: {}", e));
    }
    true
}

/// Left-click on the tray icon in `window`/`both` mode: show the popover if it
/// is hidden, hide it if it is visible.
///
/// (S13.2) The popover no longer hides itself on losing focus — clicking
/// anywhere outside it, including on its own resize border, used to close it
/// before a resize drag could even start. The tray icon and the header's ✕
/// button are now the ONLY two ways to close it, which is also what makes this
/// toggle simple: nothing else can have hidden the window between one tray
/// click and the next, so there is no race to guard against and no flag to
/// consume — a plain visibility check is the whole answer.
///
/// Returns `false` if the popover is unavailable, so the caller can fall back.
pub fn toggle_popover(app: &AppHandle, tray: RectPx) -> bool {
    let Some(state) = app.try_state::<PopoverState>() else {
        return false;
    };
    *state.tray_rect.lock() = Some(tray);

    if state.creation_failed.load(Ordering::SeqCst) {
        return false;
    }

    if let Some(w) = app.get_webview_window(POPOVER_LABEL) {
        if w.is_visible().unwrap_or(false) {
            hide_popover(app);
            return true;
        }
    }

    show_popover(app, tray)
}

/// Has webview creation already failed this session? Drives the tray's
/// session-only fallback to the native menu (W-D7).
pub fn creation_failed(app: &AppHandle) -> bool {
    app.try_state::<PopoverState>()
        .map(|s| s.creation_failed.load(Ordering::SeqCst))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920x1080 primary monitor with a 40 px taskbar at the bottom, and the
    /// tray icon sitting in it.
    fn bottom_taskbar() -> (RectPx, RectPx) {
        let work = RectPx {
            x: 0,
            y: 0,
            w: 1920,
            h: 1040,
        };
        let tray = RectPx {
            x: 1700,
            y: 1045,
            w: 24,
            h: 24,
        };
        (tray, work)
    }

    #[test]
    fn detects_each_taskbar_edge() {
        let (tray, work) = bottom_taskbar();
        assert_eq!(taskbar_edge(tray, work), TaskbarEdge::Bottom);

        let work_top = RectPx {
            x: 0,
            y: 40,
            w: 1920,
            h: 1040,
        };
        let tray_top = RectPx {
            x: 1700,
            y: 8,
            w: 24,
            h: 24,
        };
        assert_eq!(taskbar_edge(tray_top, work_top), TaskbarEdge::Top);

        let work_left = RectPx {
            x: 60,
            y: 0,
            w: 1860,
            h: 1080,
        };
        let tray_left = RectPx {
            x: 10,
            y: 900,
            w: 24,
            h: 24,
        };
        assert_eq!(taskbar_edge(tray_left, work_left), TaskbarEdge::Left);

        let work_right = RectPx {
            x: 0,
            y: 0,
            w: 1860,
            h: 1080,
        };
        let tray_right = RectPx {
            x: 1880,
            y: 900,
            w: 24,
            h: 24,
        };
        assert_eq!(taskbar_edge(tray_right, work_right), TaskbarEdge::Right);
    }

    #[test]
    fn auto_hidden_taskbar_falls_back_to_the_nearest_edge() {
        // Auto-hide: the work area is the WHOLE screen, so the tray rect is
        // inside it and no edge can be read off the geometry.
        let work = RectPx {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let tray = RectPx {
            x: 1700,
            y: 1050,
            w: 24,
            h: 24,
        };
        assert_eq!(taskbar_edge(tray, work), TaskbarEdge::Bottom);
    }

    #[test]
    fn popover_sits_above_a_bottom_taskbar_and_inside_the_work_area() {
        let (tray, work) = bottom_taskbar();
        let (x, y) = popover_origin(tray, work, 400, 560);
        assert_eq!(y, work.bottom() - 560 - EDGE_GAP, "anchored to the work-area bottom");
        assert!(y + 560 <= work.bottom(), "must never overlap the taskbar");
        assert!(x + 400 <= work.right(), "must not spill off the right edge");
        assert!(x >= work.x);
    }

    #[test]
    fn popover_is_clamped_when_the_tray_is_in_the_far_corner() {
        // Centering on a tray icon at x=1900 would put the window's right edge
        // at ~2100, i.e. 180 px off-screen.
        let work = RectPx {
            x: 0,
            y: 0,
            w: 1920,
            h: 1040,
        };
        let tray = RectPx {
            x: 1900,
            y: 1045,
            w: 16,
            h: 16,
        };
        let (x, _) = popover_origin(tray, work, 400, 560);
        assert_eq!(x, work.right() - 400 - EDGE_GAP);
    }

    #[test]
    fn popover_handles_a_monitor_left_of_the_primary_negative_coordinates() {
        // A second monitor to the LEFT of the primary has negative virtual-desktop
        // coordinates; clamping against 0 instead of the work area would throw the
        // window onto the wrong screen.
        let work = RectPx {
            x: -1920,
            y: 0,
            w: 1920,
            h: 1040,
        };
        let tray = RectPx {
            x: -300,
            y: 1045,
            w: 24,
            h: 24,
        };
        let (x, y) = popover_origin(tray, work, 400, 560);
        assert!(x >= work.x + EDGE_GAP, "x={} must stay on the left monitor", x);
        assert!(x + 400 <= work.right());
        assert_eq!(y, work.bottom() - 560 - EDGE_GAP);
    }

    #[test]
    fn popover_stays_visible_on_a_work_area_smaller_than_itself() {
        // A tiny/rotated screen or a huge scale factor: the window cannot fit.
        // Its top-left must remain on-screen rather than being pushed off.
        let work = RectPx {
            x: 0,
            y: 0,
            w: 320,
            h: 400,
        };
        let tray = RectPx {
            x: 300,
            y: 405,
            w: 16,
            h: 16,
        };
        let (x, y) = popover_origin(tray, work, 400, 560);
        assert_eq!((x, y), (work.x + EDGE_GAP, work.y + EDGE_GAP));
    }

    #[test]
    fn side_taskbar_anchors_horizontally_and_aligns_to_the_icon() {
        let work = RectPx {
            x: 0,
            y: 0,
            w: 1860,
            h: 1080,
        };
        let tray = RectPx {
            x: 1880,
            y: 500,
            w: 24,
            h: 24,
        };
        let (x, y) = popover_origin(tray, work, 400, 560);
        assert_eq!(x, work.right() - 400 - EDGE_GAP);
        assert_eq!(y, tray.center_y() - 280);
    }
}
