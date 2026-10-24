//! The island: a now-playing pill at the top of the screen.
//!
//! While the main window is minimized, this window is a thin strip at
//! the top center of the screen: nothing is painted there, and it waits.
//! When the pointer finds it the window grows into the pill and the pill
//! slides in; clicking the pill opens it into the player card, the way
//! the reference component does. Leaving folds it back, first the card,
//! then the pill, then nothing.
//!
//! The morph is one number, exactly as the reference: a single quart-out
//! tween drives every position, size, radius and type size, and a second
//! linear tween gives the close its gather. Geometry ported from the
//! reference: 260 wide, 78 shut, 189 open, a 40 to 64 sleeve, and the
//! box's corner derived from the sleeve's so the two stay concentric.
//!
//! The window cannot blur the desktop behind it, so the pane is the
//! app's own surface colour, mostly opaque; the reference's hairline and
//! hover states are kept.

use egui::{
    Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, StrokeKind, Ui, Vec2, ViewportCommand,
    pos2, vec2,
};

use crate::app::App;
use crate::i18n::gettext;
use crate::model::Action;
use crate::theme::{self, Icon};

// ── The reference's geometry, value for value ───────────────
/// The width, for both states and the frame.
pub const W: f32 = 260.0;
/// The bar, and the card.
const SHUT: f32 = 78.0;
const OPEN: f32 = 189.0;
/// One margin for everything.
const PAD: f32 = 10.0;
/// The sleeve at both ends.
const ART_SHUT: f32 = 40.0;
const ART_OPEN: f32 = 64.0;
/// The sleeve's corner on the card; the box's corner follows it.
const CORNER: f32 = 16.0;
/// The card's stack, measured from the sleeve down.
const RAIL_GAP: f32 = 22.0;
const CLOCK_GAP: f32 = 8.0;
const OPS_GAP: f32 = 16.0;
const RAIL_H: f32 = 3.0;
const CLOCK_H: f32 = 10.0;
const LEAD: f32 = 46.0;
/// The heart's box.
const LIKE: f32 = 30.0;
/// The transport's own bottom, and one more margin under it.
const RAIL_Y: f32 = PAD + ART_OPEN + RAIL_GAP;
const OPS_Y: f32 = RAIL_Y + RAIL_H + CLOCK_GAP + CLOCK_H + OPS_GAP + LEAD / 2.0;

/// The tuned duration, in milliseconds.
const BASE_MS: f64 = 460.0;
/// The morph speed knob, 0..100: 50 is the tuned 460 ms, the ends 736
/// and 184.
const MORPH: f32 = 50.0;

/// The resting strip: the whole window until the pointer finds it.
pub const RESTING_SIZE: Vec2 = vec2(W, 8.0);
/// The window's size with the pill visible, and with the card open.
pub const PILL_SIZE: Vec2 = vec2(W, SHUT);
/// The largest the window ever grows. The window is created with this as
/// its maximum inner size: a maximum pinned to the resting strip makes
/// Windows refuse every resize, which is what left the island 8 points
/// tall while it tried to draw a 78-point pill.
pub const CARD_SIZE: Vec2 = vec2(W, OPEN);
/// How long the pointer may be away before each fold, and the room a
/// hover keeps around the shape.
const GRACE_S: f64 = 0.5;
const CARD_GRACE_S: f64 = 0.7;
const POINTER_PAD: f32 = 6.0;

/// The play mark's points, lucide's own: pause is a pair of bars, and
/// play is that pair with the inner edges pulled to the middle. Wound
/// the same way in both or the halves would turn inside out.
const PAUSE_L: [f32; 8] = [6.0, 4.0, 10.0, 4.0, 10.0, 20.0, 6.0, 20.0];
const PLAY_L: [f32; 8] = [6.5, 4.0, 13.25, 8.0, 13.25, 16.0, 6.5, 20.0];
const PAUSE_R: [f32; 8] = [14.0, 4.0, 18.0, 4.0, 18.0, 20.0, 14.0, 20.0];
const PLAY_R: [f32; 8] = [13.25, 8.0, 20.0, 12.0, 20.0, 12.0, 13.25, 16.0];

fn window_size_for(visible: bool, open: bool) -> Vec2 {
    if open {
        CARD_SIZE
    } else if visible {
        PILL_SIZE
    } else {
        RESTING_SIZE
    }
}

/// The size the window should be born with, for whoever creates it. It is
/// always the card's: the island changes its window region, not its size,
/// because a resize makes Windows present a fresh surface for a frame and
/// that frame is the white flash the user saw on every hover and click.
pub fn wanted_size(_state: &IslandState) -> Vec2 {
    CARD_SIZE
}

/// The morph's speed multiplier.
fn rate(morph: f32) -> f64 {
    (1.6 - (morph / 100.0) * 1.2) as f64
}

fn mix(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Quart out: quick off the mark, landing without passing the target.
fn quart(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(4)
}

#[derive(Clone, Copy, Default)]
enum Ease {
    #[default]
    Quart,
    Linear,
    /// In and out, for the mark that leaves one shape as it arrives at
    /// the other.
    Swing,
}

impl Ease {
    fn at(self, t: f32) -> f32 {
        match self {
            Ease::Quart => quart(t),
            Ease::Linear => t,
            Ease::Swing => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// One eased value moving in time, read off the frame clock so tests
/// drive it the way the pointer does. Retargeting reads the value's
/// current place, so turning around mid-flight is continuous.
#[derive(Clone, Copy, Default)]
struct Tween {
    from: f32,
    to: f32,
    start: f64,
    dur: f64,
    ease: Ease,
}

impl Tween {
    fn value(&self, now: f64) -> f32 {
        if self.dur <= 0.0 {
            return self.to;
        }
        let t = clamp01(((now - self.start) / self.dur) as f32);
        self.from + (self.to - self.from) * self.ease.at(t)
    }

    fn retarget(&mut self, to: f32, dur: f64, ease: Ease, now: f64) {
        if self.to == to && self.dur > 0.0 {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.start = now;
        self.dur = dur;
        self.ease = ease;
    }

    /// Start over from the beginning, for a flourish that may repeat.
    fn replay(&mut self, to: f32, dur: f64, ease: Ease, now: f64) {
        self.from = 0.0;
        self.to = to;
        self.start = now;
        self.dur = dur;
        self.ease = ease;
    }

    fn done(&self, now: f64) -> bool {
        self.dur <= 0.0 || now - self.start >= self.dur
    }

    fn settled(&self, now: f64) -> bool {
        self.done(now) && self.from == self.to
    }
}

/// The island's own state: what the strip, the pill and the card are
/// doing. It lives on the app so the frame loop and the tests see one
/// story.
#[derive(Default)]
pub struct IslandState {
    /// The pointer found the strip: the pill is here or arriving.
    pub visible: bool,
    /// The card is open.
    pub open: bool,
    /// 0..1 as the pill slides in from above.
    appear: Tween,
    /// 0 shut, 1 open: the reference's one number.
    morph: Tween,
    /// The close-only gather's linear clock.
    gather: Tween,
    /// 0 pause to 1 play, for the one mark that morphs.
    mark: Tween,
    /// What the mark last followed, to spot the change.
    mark_playing: Option<bool>,
    /// The heart's beat, run when the like lands.
    beat: Tween,
    /// Whether the song was saved, to spot the like's arrival.
    was_saved: bool,
    /// When the pointer was last on the island, for the graces.
    seen_at: f64,
    /// The window region currently applied, and the one the last painted
    /// pass asked for. The applied region trails the shape by one pass so
    /// the surface already holds the newer pixels when it grows.
    #[cfg(windows)]
    region_set: bool,
    #[cfg(windows)]
    region_shown: [f32; 3],
    #[cfg(windows)]
    region_pending: [f32; 3],
    /// Whether DWM's window transitions were turned off for this window.
    #[cfg(windows)]
    transitions_off: bool,
    /// Set once the window has been parked at the top of its monitor.
    placed: bool,
}

impl IslandState {
    /// A new island window has its own place: the one before it was parked
    /// by its own frame, and a replacement must be parked again rather than
    /// trust that.
    pub fn forget_placement(&mut self) {
        self.placed = false;
    }

    /// The pill, fully settled, for a screenshot.
    pub fn settle_pill(&mut self) {
        self.visible = true;
        self.open = false;
        self.seen_at = f64::INFINITY;
        self.appear = Tween {
            from: 1.0,
            to: 1.0,
            ..Tween::default()
        };
    }

    /// The card, fully settled, for a screenshot or a test. The pointer
    /// grace never fires: a settled island is one nothing is leaving.
    pub fn settle_open(&mut self) {
        self.visible = true;
        self.open = true;
        self.seen_at = f64::INFINITY;
        self.appear = Tween {
            from: 1.0,
            to: 1.0,
            ..Tween::default()
        };
        self.morph = Tween {
            from: 1.0,
            to: 1.0,
            ..Tween::default()
        };
        self.gather = Tween {
            from: 1.0,
            to: 1.0,
            ..Tween::default()
        };
    }
}

/// The whole window, drawn into the root.
pub fn show(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    super::keys::handle(app, &ctx);
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        app.actions.push(Action::IslandToMain);
    }
    let now = ctx.input(|input| input.time);
    let pointer = ctx.input(|input| input.pointer.latest_pos());
    let window = ui.max_rect();
    if !app.island.placed {
        // One line per island window: what the drawing is told it has, and
        // what the viewport believes, so a mismatch is visible in the log.
        log::info!(
            "island: ui {:?} clip {:?} screen {:?} inner {:?}",
            window,
            ui.clip_rect(),
            ctx.viewport_rect(),
            ctx.input(|input| input.viewport().inner_rect),
        );
    }
    place_window(&ctx, app);
    // The window keeps the card's size; its region is the shape, and the
    // region is applied one pass late. Windows composites a region change
    // against the surface it already holds, so growing the region on the
    // same pass that paints the shape showed a frame of white; the surface
    // now already holds the shape's pixels when the region grows.
    #[cfg(windows)]
    if app.window_hwnd != 0 {
        if !app.island.transitions_off {
            // DWM's window transitions can also flash a freshly shown
            // surface; the island has no use for them.
            crate::window::disable_transitions(app.window_hwnd);
            app.island.transitions_off = true;
        }
        let scale = ctx.pixels_per_point();
        let shape = region_shape(app, now);
        if app.island.region_set && shape[1] < app.island.region_shown[1] {
            // A shrinking region cannot expose a row, so it is safe to
            // apply at once: the fold moves the pane up and the region must
            // follow it or the empty top stays inside the window.
            apply_region(app.window_hwnd, shape, scale);
            app.island.region_shown = shape;
        } else if !app.island.region_set {
            // The first region is set only once a shape has been painted
            // and presented: setting a region before Windows has drawn the
            // window is a known source of a white flicker.
            if app.island.region_pending != [0.0; 3] {
                apply_region(app.window_hwnd, app.island.region_pending, scale);
                app.island.region_shown = app.island.region_pending;
                app.island.region_set = true;
            }
        } else if app.island.region_pending != app.island.region_shown {
            apply_region(app.window_hwnd, app.island.region_pending, scale);
            app.island.region_shown = app.island.region_pending;
        }
        app.island.region_pending = shape;
    }
    step(app, &ctx, window, pointer, now);
    paint(app, ui, window, now);
}

/// The window region the island's shape needs, in points: width, height and
/// corner radius. While the pill slides in or out, the region follows the
/// pane's visible bottom, not its full height: the fold lifts the pane up,
/// and a region that kept the full height left an empty strip at the top of
/// the window, which flashed white as it composed.
#[cfg(windows)]
fn region_shape(app: &App, now: f64) -> [f32; 3] {
    let p = app.island.morph.value(now);
    let appear = app.island.appear.value(now);
    let shown = app.island.visible || appear > 0.001;
    if shown {
        let lift = (appear - 1.0) * (SHUT + 12.0);
        let height = (lift + mix(SHUT, OPEN, p)).max(RESTING_SIZE.y);
        [W, height, mix(20.0, CORNER + PAD, p)]
    } else {
        [W, RESTING_SIZE.y, 4.0]
    }
}

#[cfg(windows)]
fn apply_region(hwnd: isize, shape: [f32; 3], scale: f32) {
    crate::window::set_round_region(
        hwnd,
        (shape[0] * scale).round() as i32,
        (shape[1] * scale).round() as i32,
        (shape[2] * scale).round() as i32,
    );
}

/// Parks the window at the top center of its monitor, once the window
/// manager has said where the window is.
fn place_window(ctx: &egui::Context, app: &mut App) {
    if app.island.placed {
        return;
    }
    let viewport = ctx.input(|input| input.viewport().clone());
    let (Some(_outer), Some(monitor)) = (viewport.outer_rect, viewport.monitor_size) else {
        return;
    };
    app.island.placed = true;
    // The virtual screen's primary monitor sits at the origin, which is
    // where a single-monitor setup lives. On Windows the client area of a
    // borderless window starts one pixel below its outer rectangle, so the
    // window is parked one pixel higher: otherwise that row shows the
    // desktop as a bright sliver above the pill.
    #[cfg(windows)]
    let top = -1.0;
    #[cfg(not(windows))]
    let top = 0.0;
    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2(
        (monitor.x - W) / 2.0,
        top,
    )));
}

/// The hover and click story: the strip wakes the pill, the pill opens
/// the card, and leaving folds it back the way it came.
fn step(app: &mut App, ctx: &egui::Context, window: Rect, pointer: Option<Pos2>, now: f64) {
    let zone = Rect::from_min_size(window.min, window_size_for(app.island.visible, app.island.open))
        .expand(POINTER_PAD);
    let hovered = pointer.is_some_and(|p| zone.contains(p));
    if hovered {
        app.island.seen_at = now;
    }
    // Control is read from the keyboard itself where the platform allows
    // it, because a modifier only arrives as an event when the window has
    // focus, and the island usually does not.
    let ctrl_down = ctrl_held(ctx);
    // The strip only answers a pointer that is holding Control: passing the
    // top of the screen by accident must not bring the island out. While the
    // pointer rests on the strip the island keeps waking, so pressing
    // Control after arriving is noticed without moving the pointer again.
    if !app.island.visible && hovered && ctrl_down {
        app.island.visible = true;
    } else if !app.island.visible && hovered {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    // Only a pointer that is still there starts the slide, so a fold is
    // never restarted by this.
    if hovered && app.island.visible && app.island.appear.to < 1.0 {
        app.island.appear.retarget(1.0, 0.32, Ease::Quart, now);
    }

    let away = now - app.island.seen_at;
    if app.island.visible && !app.island.open && away > GRACE_S && app.island.appear.done(now) {
        app.island.appear.retarget(0.0, 0.22, Ease::Quart, now);
        if app.island.appear.done(now) {
            app.island.visible = false;
        }
    }
    if app.island.open && away > CARD_GRACE_S {
        close_card(app, now);
    }

    let moving = !app.island.appear.settled(now)
        || !app.island.morph.settled(now)
        || !app.island.gather.settled(now)
        || !app.island.mark.settled(now)
        || !app.island.beat.settled(now);
    if moving {
        // The island's animations are slow enough for 30 frames a second;
        // an immediate repaint every pass spent twice the GPU on them.
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

/// Whether Control is held, whether or not this window has the keyboard
/// focus. A focused window reports it as a modifier; the island usually has
/// no focus, so Windows is asked for the key's own state instead.
fn ctrl_held(ctx: &egui::Context) -> bool {
    if ctx.input(|input| input.modifiers.ctrl) {
        return true;
    }
    #[cfg(windows)]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn GetAsyncKeyState(key: i32) -> i16;
        }
        const VK_CONTROL: i32 = 0x11;
        // The high bit is set while the key is down.
        return unsafe { (GetAsyncKeyState(VK_CONTROL) as u16 & 0x8000) != 0 };
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Opens the card.
fn open_card(app: &mut App, now: f64) {
    let dur = BASE_MS / 1000.0 * rate(MORPH);
    app.island.open = true;
    app.island.morph.retarget(1.0, dur, Ease::Quart, now);
    app.island.gather.retarget(1.0, dur, Ease::Linear, now);
}

/// Folds the card back, with the close-only gather.
fn close_card(app: &mut App, now: f64) {
    let dur = BASE_MS / 1000.0 * rate(MORPH);
    app.island.open = false;
    app.island.morph.retarget(0.0, dur, Ease::Quart, now);
    app.island.gather.retarget(0.0, dur, Ease::Linear, now);
    app.island.seen_at = now;
}

/// The pill's box, sliding in from above as the pointer finds it, with
/// the close's gather scaling the whole object about the top center.
fn box_rect(appear: f32, p: f32, gather: f32, open: bool) -> Rect {
    let y = (appear - 1.0) * (SHUT + 12.0);
    let rect = Rect::from_min_size(pos2(0.0, y), vec2(W, mix(SHUT, OPEN, p)));
    let swell = (std::f32::consts::PI * clamp01(gather).powf(1.5)).sin();
    let zoom = if open { 1.0 } else { 1.0 - 0.035 * swell };
    let anchor = pos2(rect.center().x, rect.top());
    Rect::from_min_max(
        anchor + (rect.min - anchor) * zoom,
        anchor + (rect.max - anchor) * zoom,
    )
}

/// The ridge between the pill and the card: the tap that opens is the
/// whole bar shut, and the sleeve and words once it is open.
pub(crate) fn tap_rect(box_rect: Rect, p: f32) -> Rect {
    let art_s = mix(ART_SHUT, ART_OPEN, p);
    if p < 0.5 {
        box_rect
    } else {
        Rect::from_min_size(
            box_rect.min + vec2(PAD, PAD),
            vec2(art_s + 10.0 + mix(94.0, 126.0, p), art_s),
        )
    }
}

/// The transport's three buttons, in the row's order.
pub(crate) fn ops_rects(box_rect: Rect, p: f32) -> [Rect; 3] {
    let side = mix(24.0, 34.0, p);
    let lead = mix(30.0, LEAD, p);
    let gap = mix(5.0, 14.0, p);
    let total = side * 2.0 + lead + gap * 2.0;
    let center = pos2(
        box_rect.left() + mix(W - PAD - 44.0, W / 2.0, p),
        box_rect.top() + mix(PAD + ART_SHUT / 2.0, OPS_Y, p),
    );
    let mut x = center.x - total / 2.0;
    let mut next = |size: f32| {
        let rect = Rect::from_center_size(pos2(x + size / 2.0, center.y), Vec2::splat(size));
        x += size + gap;
        rect
    };
    [next(side), next(lead), next(side)]
}

/// The way back to the app, at the end of the transport row where the
/// design leaves room.
pub(crate) fn restore_rect(box_rect: Rect, p: f32) -> Rect {
    let ops = ops_rects(box_rect, p);
    Rect::from_center_size(
        pos2(box_rect.right() - PAD - 15.0, ops[2].center().y),
        Vec2::splat(30.0),
    )
}

/// The heart, the card's top-right corner.
pub(crate) fn like_rect(box_rect: Rect) -> Rect {
    Rect::from_min_size(
        pos2(box_rect.right() - PAD - LIKE, box_rect.top() + PAD),
        Vec2::splat(LIKE),
    )
}

fn paint(app: &mut App, ui: &mut Ui, window: Rect, now: f64) {
    let (p, appear, gather, late, open, visible) = {
        let island = &app.island;
        let p = island.morph.value(now);
        (
            p,
            island.appear.value(now),
            island.gather.value(now),
            clamp01((p - 0.6) / 0.4),
            island.open,
            island.visible,
        )
    };
    let palette = app.palette;
    let pane = if palette.dark {
        Color32::from_rgb(22, 24, 28)
    } else {
        Color32::from_rgb(250, 250, 250)
    };
    if !visible && appear <= 0.001 {
        return;
    }
    let local = box_rect(appear, p, gather, open);
    let rect = local.translate(window.min.to_vec2());

    let hairline = if palette.dark {
        Color32::from_white_alpha(26)
    } else {
        Color32::from_black_alpha(18)
    };
    let fade = |c: Color32| c.gamma_multiply(clamp01(appear));

    // The one thing to press to change shape, before the controls so a
    // press on them wins. Shut it is the whole bar; open, the sleeve and
    // the words, so reaching for the title does not fold the player.
    let tap = if open { tap_rect(rect, p) } else { rect };
    let tap_response = ui.interact(tap, egui::Id::new("island-tap"), egui::Sense::click());
    if tap_response.clicked() {
        if open {
            close_card(app, now);
        } else {
            open_card(app, now);
        }
    }

    let radius = mix(20.0, CORNER + PAD, p).round() as u8;
    ui.painter().rect_filled(rect, radius, fade(pane));
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, fade(hairline)),
        StrokeKind::Inside,
    );

    paint_cover(app, ui, rect, p);
    paint_words(app, ui, rect, p);
    paint_track(app, ui, rect, p, late);
    paint_transport(app, ui, rect, p, now);
    paint_heart(app, ui, rect, p, late, now);
}

/// The hinge: the sleeve, big enough to be the thing the eye tracks.
fn paint_cover(app: &mut App, ui: &Ui, rect: Rect, p: f32) {
    let art_s = mix(ART_SHUT, ART_OPEN, p);
    let art = Rect::from_min_size(rect.min + vec2(PAD, PAD), Vec2::splat(art_s));
    let palette = app.palette;
    let url = app
        .now_playing()
        .and_then(|now| app.small_art(&now, art_s).map(str::to_string));
    super::widgets::playing_cover(
        ui,
        &palette,
        app.backend.art(),
        "island",
        url.as_deref(),
        art,
        mix(10.0, CORNER, p),
    );
}

/// Title and artist, beside the sleeve at both ends.
fn paint_words(app: &mut App, ui: &Ui, rect: Rect, p: f32) {
    let palette = app.palette;
    let (title_text, by_text) = match app.now_playing() {
        Some(now) => {
            let by = if now.artists.is_empty() {
                now.subtitle.clone()
            } else {
                now.artists
                    .iter()
                    .map(|artist| artist.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            (now.title.clone(), by)
        }
        None => (
            "Musify".to_string(),
            gettext(app.locale, "Nothing playing").into_owned(),
        ),
    };
    let painter = ui.painter();
    let art_s = mix(ART_SHUT, ART_OPEN, p);
    let left = rect.left() + PAD + art_s + 10.0;
    let width = mix(94.0, W - PAD - (PAD + ART_OPEN + 10.0) - (LIKE + 10.0), p);
    let title = crate::bidi::layout(
        painter,
        &title_text,
        theme::medium(mix(13.0, 15.5, p)),
        palette.text,
        width,
        1,
        Some(crate::bidi::ELLIPSIS),
    );
    let by = crate::bidi::layout(
        painter,
        &by_text,
        theme::regular(mix(11.0, 12.0, p)),
        palette.secondary,
        width,
        1,
        None,
    );
    // Centred by the sleeve: the words' box is the sleeve's height, and
    // the two lines centre inside that.
    let title_h = title.size().y;
    let total = title_h + 2.0 + by.size().y;
    let top = rect.top() + PAD + (art_s - total) / 2.0;
    painter.galley(pos2(left, top), title, palette.text);
    painter.galley(pos2(left, top + title_h + 2.0), by, palette.secondary);
}

/// The track: a rail along the foot of the bar, the times arriving on
/// the deep half of the morph.
fn paint_track(app: &mut App, ui: &Ui, rect: Rect, p: f32, late: f32) {
    let palette = app.palette;
    let painter = ui.painter();
    let y = rect.top() + mix(SHUT - PAD - RAIL_H, RAIL_Y, p);
    let track = Rect::from_min_size(pos2(rect.left() + PAD, y), vec2(W - PAD * 2.0, RAIL_H));
    let rail = if palette.dark {
        Color32::from_white_alpha(30)
    } else {
        Color32::from_black_alpha(30)
    };
    painter.rect_filled(track, 1.0, rail);
    let (position, duration) = app
        .now_playing()
        .map(|now| (now.position_ms, now.duration_ms))
        .unwrap_or((0, 0));
    let fraction = if duration > 0 {
        (position as f32 / duration as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if fraction > 0.0 {
        let run = Rect::from_min_size(track.min, vec2(track.width() * fraction, RAIL_H));
        painter.rect_filled(run, 1.0, palette.text);
    }
    let clock_color = palette.secondary.gamma_multiply(late);
    let font: FontId = theme::regular(11.0);
    painter.text(
        pos2(rect.left() + PAD, y + RAIL_H + CLOCK_GAP),
        Align2::LEFT_TOP,
        playtime(position),
        font.clone(),
        clock_color,
    );
    painter.text(
        pos2(rect.right() - PAD, y + RAIL_H + CLOCK_GAP),
        Align2::RIGHT_TOP,
        format!("\u{2212}{}", playtime(duration.saturating_sub(position))),
        font,
        clock_color,
    );
}

/// Restart, play, next, and the way home.
fn paint_transport(app: &mut App, ui: &mut Ui, rect: Rect, p: f32, now: f64) {
    let palette = app.palette;
    let control = app.now_playing().is_some_and(|now| now.can_control);
    let playing = app.now_playing().is_some_and(|now| now.playing);
    // The one mark that morphs follows the state on its own clock.
    if app.island.mark_playing != Some(playing) {
        app.island.mark_playing = Some(playing);
        app.island
            .mark
            .retarget(if playing { 0.0 } else { 1.0 }, 0.30, Ease::Swing, now);
    }
    let mark_t = app.island.mark.value(now);
    let ops = ops_rects(rect, p);
    let icons = if palette.dark {
        Color32::from_white_alpha(150)
    } else {
        Color32::from_black_alpha(140)
    };
    let ink = palette.text;

    // Restart.
    if let Some(response) = round_button(ui, ops[0], "island-restart") {
        theme::paint_icon(
            ui,
            Icon::SkipBack,
            ops[0],
            mix(14.0, 16.0, p),
            if response.hovered() { ink } else { icons },
        );
        if response.clicked() && control {
            app.actions.push(Action::Seek(0));
        }
    }

    // Play: the only filled mark, the one you press most.
    let lead = ops[1];
    let response = ui.interact(lead, egui::Id::new("island-play"), egui::Sense::click());
    let fill = if !control {
        ink.gamma_multiply(0.35)
    } else if response.hovered() {
        ink.gamma_multiply(0.88)
    } else {
        ink
    };
    ui.painter()
        .circle_filled(lead.center(), lead.width() / 2.0, fill);
    paint_mark(ui, lead, mix(14.0, 18.0, p), mark_t, &palette);
    if response.clicked() && control {
        app.actions.push(Action::TogglePlay);
    }

    // Next.
    if let Some(response) = round_button(ui, ops[2], "island-next") {
        theme::paint_icon(
            ui,
            Icon::SkipForward,
            ops[2],
            mix(14.0, 16.0, p),
            if response.hovered() { ink } else { icons },
        );
        if response.clicked() && control {
            app.actions.push(Action::Next);
        }
    }

    // The way back to the app, where the design leaves room.
    if p > 0.5
        && let Some(response) = round_button(ui, restore_rect(rect, p), "island-restore")
    {
        theme::paint_icon(
            ui,
            Icon::Expand,
            restore_rect(rect, p),
            15.0,
            if response.hovered() { ink } else { icons },
        );
        response.clone().on_hover_text(
            gettext(app.locale, "Open the main window").as_ref(),
        );
        if response.clicked() {
            app.actions.push(Action::IslandToMain);
        }
    }
}

/// The heart, and the beat when the like lands.
fn paint_heart(app: &mut App, ui: &mut Ui, rect: Rect, _p: f32, late: f32, now: f64) {
    let Some(now_playing) = app.now_playing() else {
        return;
    };
    if now_playing.is_episode || late <= 0.01 {
        return;
    }
    let saved = app.is_saved(&now_playing.uri).unwrap_or(false);
    if saved && !app.island.was_saved {
        app.island.beat.replay(1.0, 0.34, Ease::Linear, now);
    }
    app.island.was_saved = saved;

    let area = like_rect(rect);
    let response = ui.interact(area, egui::Id::new("island-like"), egui::Sense::click());
    if response.hovered() {
        let fill = if app.palette.dark {
            Color32::from_white_alpha((20.0 * late) as u8)
        } else {
            Color32::from_black_alpha((12.0 * late) as u8)
        };
        ui.painter().circle_filled(area.center(), 15.0, fill);
    }
    let bump = (std::f32::consts::PI * clamp01(app.island.beat.value(now))).sin();
    let color = if saved {
        app.palette.text
    } else {
        app.palette.secondary
    }
    .gamma_multiply(late);
    let icon = if saved { Icon::HeartFilled } else { Icon::Heart };
    theme::paint_icon(ui, icon, area, 16.0 * (1.0 + 0.34 * bump), color);
    if response.clicked() {
        app.actions.push(Action::ToggleSaved(now_playing.uri.clone()));
    }
}

/// A round hover button.
fn round_button(ui: &mut Ui, area: Rect, id: &str) -> Option<egui::Response> {
    let response = ui.interact(area, egui::Id::new(id), egui::Sense::click());
    if response.hovered() {
        let fill = if ui.visuals().dark_mode {
            Color32::from_white_alpha(20)
        } else {
            Color32::from_black_alpha(12)
        };
        ui.painter()
            .circle_filled(area.center(), area.width() / 2.0, fill);
    }
    Some(response)
}

/// The play mark, drawn rather than swapped: both marks are the same two
/// quadrilaterals, and the goo bulges the way between them. Ported from
/// the reference's points and its pull, tilt and squash.
fn paint_mark(ui: &Ui, area: Rect, size: f32, t: f32, palette: &theme::Palette) {
    let t = clamp01(t);
    let goo = (t * std::f32::consts::PI).sin();
    let pull = 1.6 * goo;
    let tip = if t < 0.5 { 9.0 } else { -9.0 } * goo * std::f32::consts::PI / 180.0;
    let center = area.center();
    let scale = size / 24.0;
    let on_ink = palette.window;
    let quad = |a: &[f32; 8], b: &[f32; 8], shift: f32| {
        let mut points = Vec::with_capacity(4);
        for i in (0..8).step_by(2) {
            let x = mix(a[i], b[i], t) - 12.0 + shift;
            let y = mix(a[i + 1], b[i + 1], t) - 12.0;
            let (s, c) = (tip.sin(), tip.cos());
            let rx = x * c - y * s;
            let ry = x * s + y * c;
            points.push(
                center
                    + vec2(
                        rx * scale * (1.0 - 0.13 * goo),
                        ry * scale * (1.0 + 0.11 * goo),
                    ),
            );
        }
        points
    };
    let stroke = Stroke::new((2.0 * scale).max(1.0), on_ink);
    ui.painter()
        .add(Shape::convex_polygon(quad(&PAUSE_L, &PLAY_L, pull), on_ink, stroke));
    ui.painter()
        .add(Shape::convex_polygon(quad(&PAUSE_R, &PLAY_R, -pull), on_ink, stroke));
}

/// `3:41`.
fn playtime(ms: u32) -> String {
    format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_knob_hits_the_reference_ends() {
        assert!((rate(50.0) - 1.0).abs() < 1e-6, "50 is the tuned 460 ms");
        assert!((rate(0.0) - 1.6).abs() < 1e-6);
        assert!((rate(100.0) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn the_window_sizes_are_the_reference_geometry() {
        assert_eq!(RESTING_SIZE, vec2(260.0, 8.0));
        assert_eq!(PILL_SIZE, vec2(260.0, 78.0));
        assert_eq!(CARD_SIZE, vec2(260.0, 189.0));
        assert!((RAIL_Y - 96.0).abs() < 1e-6);
        assert!((OPS_Y - 156.0).abs() < 1e-6);
        assert!((OPEN - (OPS_Y + LEAD / 2.0 + PAD)).abs() < 1e-6);
    }

    #[test]
    fn the_morph_turns_around_where_it_is() {
        let mut tween = Tween::default();
        tween.retarget(1.0, 0.46, Ease::Quart, 10.0);
        assert_eq!(tween.value(10.0), 0.0);
        assert!(tween.value(10.23) > 0.5, "quart-out is quick off the mark");
        assert_eq!(tween.value(10.46), 1.0);
        // A second press mid-flight reads its current place, not an end
        // it never reached.
        tween.retarget(0.0, 0.46, Ease::Quart, 10.23);
        let at = tween.value(10.23);
        assert!((tween.from - at).abs() < 1e-6, "continuous turn-around");
    }

    #[test]
    fn the_gather_is_zero_at_both_ends_of_the_close() {
        let gather = |u: f32| (std::f32::consts::PI * clamp01(u).powf(1.5)).sin();
        assert!(gather(0.0).abs() < 1e-6);
        assert!(gather(1.0).abs() < 1e-6);
        assert!(gather(0.63) > 0.9, "the swell peaks late");
    }

    #[test]
    fn the_late_window_holds_the_card_only_things_back() {
        let late = |p: f32| clamp01((p - 0.6) / 0.4);
        assert!(late(0.0).abs() < 1e-6);
        assert!(late(0.6).abs() < 1e-6);
        assert!((late(0.8) - 0.5).abs() < 1e-6);
        assert!((late(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_transport_slides_from_the_end_to_the_middle() {
        let bar = Rect::from_min_size(Pos2::ZERO, vec2(W, SHUT));
        let shut = ops_rects(bar, 0.0);
        // Shut, the row sits at the bar's right end, small.
        assert!(shut[1].center().x > W - 80.0);
        assert!((shut[1].width() - 30.0).abs() < 1e-6);
        let card = Rect::from_min_size(Pos2::ZERO, vec2(W, OPEN));
        let open = ops_rects(card, 1.0);
        // Open, the play is centred under the sleeve.
        assert!((open[1].center().x - W / 2.0).abs() < 0.5);
        assert!((open[1].width() - LEAD).abs() < 1e-6);
        assert!((open[1].center().y - OPS_Y).abs() < 1e-6);
    }

    #[test]
    fn the_mark_moves_between_its_two_shapes() {
        let blend = |a: &[f32; 8], b: &[f32; 8], t: f32| {
            let mut out = [0.0f32; 8];
            for i in 0..8 {
                out[i] = mix(a[i], b[i], t);
            }
            out
        };
        let pause = blend(&PAUSE_L, &PLAY_L, 0.0);
        let play = blend(&PAUSE_L, &PLAY_L, 1.0);
        assert_eq!(pause[0], 6.0);
        assert_eq!(play[0], 6.5);
        assert!(play[2] > pause[2], "the inner edge travels to the middle");
    }

    #[test]
    fn the_box_scales_about_its_top_center() {
        // A settled open box is exactly the card.
        let settled = box_rect(1.0, 1.0, 1.0, true);
        assert!((settled.height() - OPEN).abs() < 1e-4);
        // Mid-close, the object gathers in a touch, about the top center.
        let folded = box_rect(1.0, 0.0, 0.63, false);
        assert!(folded.width() < W, "the close pulls in");
        assert!(
            (folded.center().x - W / 2.0).abs() < 1e-4,
            "about the center"
        );
        assert!((folded.top() - 0.0).abs() < 1e-4, "and the top");
    }
}
