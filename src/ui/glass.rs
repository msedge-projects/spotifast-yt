//! The liquid-glass mini player.
//!
//! The compact window the mini player button opens, in the frosted-glass
//! style of the macOS Tahoe references: a translucent panel with soft
//! blue and white light, rounded corners and hairline edges, carrying the
//! same controls as the classic player and, with the equalizer open, its
//! bands as glass sliders. It stands in for the skin-drawn classic player
//! while no custom Winamp skin is worn; a worn skin keeps that player.
//!
//! The window is transparent, so the panel's fill really shows what is
//! behind it. There is no blur behind the panel: egui cannot blur the
//! desktop, and the material is approximated with layered translucency,
//! the way the search panel's frosted fill is.

use egui::{
    Align, Align2, Color32, Id, Layout, Rect, Response, Sense, Stroke, Ui, UiBuilder, Vec2,
    ViewportCommand, pos2, vec2,
};

use crate::app::{App, NowPlaying};
use crate::i18n::{gettext, pgettext};
use crate::model::{Action, Page};
use crate::player::RepeatMode;
use crate::theme::{self, Icon};

/// The panel's width in logical points.
pub const WIDTH: f32 = 376.0;
/// The room around the panel inside its window: the rounded corners and
/// the soft shadow live there, over whatever is behind the window.
const MARGIN: f32 = 18.0;
/// The panel's height with the player alone, and the equalizer's added
/// block when it is open.
const PLAYER_HEIGHT: f32 = 178.0;
const EQ_BLOCK: f32 = 128.0;
/// The panel's margin, the gaps inside it, and the artwork's size.
const PAD: f32 = 14.0;
const GAP: f32 = 10.0;
const ART: f32 = 64.0;
/// The equalizer's slider height.
const BAND_HEIGHT: f32 = 64.0;

/// The window's wanted size, for the equalizer's state: the panel plus the
/// light around it.
pub fn window_size(settings: &crate::settings::Settings) -> Vec2 {
    let height = PLAYER_HEIGHT + if settings.eq_open { EQ_BLOCK } else { 0.0 };
    vec2(WIDTH, height) + Vec2::splat(MARGIN * 2.0)
}

/// A first guess at the window's size, before the display's scale is
/// known; the first frame corrects it. The panel is in logical points, so
/// this is already the size.
pub fn initial_size(settings: &crate::settings::Settings) -> Vec2 {
    window_size(settings)
}

/// The whole window, drawn into the root.
pub fn show(app: &mut App, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    fit_window(&ctx, &app.settings);
    super::keys::handle(app, &ctx);
    // Escape comes home, the way closing the classic player's window would.
    if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
        app.actions.push(Action::ToggleWinampWindow);
    }
    let palette = app.palette;
    let now = app.now_playing();
    let window = ui.max_rect();
    let rect = panel_rect(window);
    paint_panel(ui, &palette, rect);
    drag_anywhere(ui, window);
    playing_row(app, ui, &palette, rect, now.as_ref());
    seek_row(app, ui, &palette, rect, now.as_ref());
    transport_row(app, ui, &palette, rect, now.as_ref());
    if app.settings.eq_open {
        equalizer(app, ui, &palette, rect);
    }
}

/// Fits the window to the panel after the display scale is known.
fn fit_window(ctx: &egui::Context, settings: &crate::settings::Settings) {
    let wanted = window_size(settings);
    // Not `inner_rect`: Wayland reports no window positions, so that is
    // `None` there; the screen rect is the window's size on every desktop.
    let current = ctx.viewport_rect().size();
    if (current - wanted).abs().max_elem() < 1.0 {
        return;
    }
    // Retry rejected resize requests at most once per second.
    let asked = Id::new("glass-fit-asked");
    let last: Option<f64> = ctx.data(|data| data.get_temp(asked));
    let now = ctx.input(|input| input.time);
    if last.is_some_and(|last| now - last < 1.0) {
        return;
    }
    ctx.data_mut(|data| data.insert_temp(asked, now));
    ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(wanted));
    ctx.send_viewport_cmd(ViewportCommand::MaxInnerSize(wanted));
    ctx.send_viewport_cmd(ViewportCommand::InnerSize(wanted));
}

/// The panel inside its window's margin.
pub(crate) fn panel_rect(window: Rect) -> Rect {
    window.shrink(MARGIN)
}

/// The panel's three rows: the artwork, the seek line, and the transport.
fn rows(rect: Rect) -> (Rect, Rect, Rect) {
    let left = rect.left() + PAD;
    let width = rect.width() - PAD * 2.0;
    let art = Rect::from_min_size(pos2(left, rect.top() + PAD), Vec2::splat(ART));
    let seek = Rect::from_min_size(pos2(left, art.bottom() + GAP), vec2(width, 20.0));
    let transport = Rect::from_min_size(pos2(left, seek.bottom() + GAP), vec2(width, 44.0));
    (art, seek, transport)
}

/// The frosted panel: a soft shadow, the glass fill, its light.
fn paint_panel(ui: &Ui, palette: &theme::Palette, rect: Rect) {
    let radius = 24.0;
    // The shadow, layered so the translucent edges stay soft; it lives in
    // the window's margin, over whatever is behind.
    for (grow, alpha) in [(16.0, 12u8), (10.0, 20), (4.0, 28)] {
        ui.painter().rect_filled(
            rect.expand(grow),
            radius + grow,
            Color32::from_black_alpha(alpha),
        );
    }
    let (top, bottom) = if palette.dark {
        (
            Color32::from_rgba_unmultiplied(76, 98, 138, 152),
            Color32::from_rgba_unmultiplied(12, 18, 28, 152),
        )
    } else {
        (
            Color32::from_rgba_unmultiplied(234, 243, 255, 150),
            Color32::from_rgba_unmultiplied(255, 255, 255, 96),
        )
    };
    // The light falls from the top in quiet steps: overlapping translucent
    // bands approximate a gradient without a mesh, and no seam shows
    // because every step is faint.
    ui.painter().rect_filled(rect, radius, bottom);
    let steps = 8;
    let ink = Color32::from_rgba_unmultiplied(top.r(), top.g(), top.b(), 26);
    for step in 1..=steps {
        let t = step as f32 / steps as f32;
        let band = Rect::from_min_size(rect.min, vec2(rect.width(), rect.height() * (1.0 - t) + 8.0));
        if band.height() > 1.0 {
            ui.painter().rect_filled(band, radius, ink);
        }
    }
    // The glass's own light: a sheen at the top corner and a bright edge
    // above, a shade below, the way real glass catches a room.
    let sheen = Color32::from_white_alpha(if palette.dark { 6 } else { 9 });
    ui.painter()
        .circle_filled(pos2(rect.left() + 96.0, rect.top() + 42.0), 110.0, sheen);
    ui.painter()
        .circle_filled(pos2(rect.left() + 96.0, rect.top() + 42.0), 64.0, sheen);
    let edge = if palette.dark { 70 } else { 150 };
    let shade = if palette.dark { 40 } else { 26 };
    ui.painter().hline(
        (rect.left() + radius * 0.6)..=(rect.right() - radius * 0.6),
        rect.top() + 1.0,
        Stroke::new(1.5, Color32::from_white_alpha(edge)),
    );
    ui.painter().hline(
        (rect.left() + radius * 0.6)..=(rect.right() - radius * 0.6),
        rect.bottom() - 0.5,
        Stroke::new(1.0, Color32::from_black_alpha(shade)),
    );
    ui.painter().vline(
        rect.left() + 0.75,
        (rect.top() + radius * 0.6)..=(rect.bottom() - radius * 0.6),
        Stroke::new(1.0, Color32::from_white_alpha(edge / 2 + 20)),
    );
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, hairline(palette)),
        egui::StrokeKind::Inside,
    );
}

/// The window drags from anywhere the controls are not.
fn drag_anywhere(ui: &mut Ui, rect: Rect) {
    let response = ui.interact(rect.shrink(2.0), Id::new("glass-drag"), Sense::drag());
    if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }
}

/// The close chip's rect inside the panel.
pub(crate) fn close_rect(rect: Rect) -> Rect {
    Rect::from_min_size(
        pos2(rect.right() - PAD - 26.0, rect.top() + PAD + 6.0),
        Vec2::splat(26.0),
    )
}

/// The way-home button's rect, shown while nothing is playing.
pub(crate) fn home_pill_rect(window: Rect) -> Rect {
    let (_, _, transport) = rows(panel_rect(window));
    home_pill(transport)
}

fn home_pill(transport: Rect) -> Rect {
    Rect::from_center_size(transport.center(), vec2(216.0, 36.0))
}

/// The artwork, title, subtitle, the equalizer's chip, and the way back.
fn playing_row(app: &mut App, ui: &mut Ui, palette: &theme::Palette, rect: Rect, now: Option<&NowPlaying>) {
    let (art, _, _) = rows(rect);
    let key = now.map(|now| now.uri.as_str()).unwrap_or("nothing");
    super::widgets::playing_cover(
        ui,
        palette,
        app.backend.art(),
        key,
        now.and_then(|now| app.small_art(now, ART)),
        art,
        14.0,
    );
    let cover = ui
        .interact(art, Id::new("glass-cover"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if cover.clicked()
        && let Some(now) = now
    {
        open_current(app, now);
    }

    let close = close_rect(rect);
    let chip = Rect::from_min_size(
        pos2(close.left() - 8.0 - 34.0, rect.top() + PAD + 7.0),
        vec2(34.0, 24.0),
    );
    let text_right = chip.left() - 8.0;
    let text_rect = Rect::from_min_size(
        pos2(art.right() + 12.0, art.top() + 4.0),
        vec2((text_right - art.right() - 12.0).max(40.0), 54.0),
    );
    let mut text_ui = ui.new_child(
        UiBuilder::new()
            .max_rect(text_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    text_ui.set_clip_rect(text_rect.intersect(ui.clip_rect()));
    text_ui.spacing_mut().item_spacing.y = 2.0;
    match now {
        Some(now) => {
            let title = theme::link(&mut text_ui, &now.title, theme::medium(15.0), palette.text);
            if title.clicked() {
                open_current(app, now);
            }
            if now.artists.is_empty() {
                if theme::link(
                    &mut text_ui,
                    &now.subtitle,
                    theme::regular(12.0),
                    palette.secondary,
                )
                .clicked()
                    && let Some(id) = &now.show_id
                {
                    app.actions.push(Action::Open(Page::Show(id.clone())));
                }
            } else {
                text_ui.horizontal_top(|ui| {
                    super::widgets::artist_links(
                        ui,
                        app,
                        &now.artists,
                        theme::regular(12.0),
                        palette.secondary,
                    );
                });
            }
        }
        None => {
            theme::text(
                &mut text_ui,
                gettext(app.locale, "Nothing playing"),
                theme::medium(15.0),
                palette.dim,
            );
            theme::text(
                &mut text_ui,
                gettext(app.locale, "Open the main window to pick something"),
                theme::regular(12.0),
                palette.secondary,
            );
        }
    }

    // The equalizer's chip: the glass button that opens the bands.
    let chip_title = gettext(app.locale, "Equalizer");
    let response = glass_button(ui, chip, Id::new("glass-eq-chip"), palette)
        .on_hover_text(chip_title.as_ref());
    if app.settings.eq_open {
        ui.painter().rect_filled(
            chip,
            12.0,
            palette.accent.gamma_multiply(0.22),
        );
    }
    ui.painter().text(
        chip.center(),
        Align2::CENTER_CENTER,
        "EQ",
        theme::medium(12.5),
        palette.text,
    );
    if response.clicked() {
        app.actions.push(Action::ToggleWinampEq);
    }

    let close_title = gettext(app.locale, "Back to the main window");
    let response = glass_button(ui, close, Id::new("glass-close"), palette)
        .on_hover_text(close_title.as_ref());
    theme::paint_icon(
        ui,
        Icon::X,
        close,
        14.0,
        palette.secondary,
    );
    if response.clicked() {
        app.actions.push(Action::ToggleWinampWindow);
    }
}

/// Opens what is playing, the way clicking the title in the classic player
/// does, and comes back to the main window to show it.
fn open_current(app: &mut App, now: &NowPlaying) {
    if let Some(id) = &now.album_id {
        app.actions.push(Action::Open(Page::Album(id.clone())));
    } else if let Some(id) = &now.show_id {
        app.actions.push(Action::Open(Page::Show(id.clone())));
    }
    app.actions.push(Action::ToggleWinampWindow);
}

/// The time line: elapsed, a seekable glass track, the length.
fn seek_row(app: &mut App, ui: &mut Ui, palette: &theme::Palette, rect: Rect, now: Option<&NowPlaying>) {
    let (_, seek, _) = rows(rect);
    let painter = ui.painter().clone();
    let time_font = theme::regular(11.0);
    let position = now.map_or(0, |now| now.position_ms);
    let duration = now.map_or(0, |now| now.duration_ms);
    painter.text(
        pos2(seek.left() + 2.0, seek.center().y),
        Align2::LEFT_CENTER,
        playtime(position),
        time_font.clone(),
        palette.secondary,
    );
    painter.text(
        pos2(seek.right() - 2.0, seek.center().y),
        Align2::RIGHT_CENTER,
        playtime(duration),
        time_font,
        palette.secondary,
    );
    let track = Rect::from_center_size(seek.center(), vec2((seek.width() - 96.0).max(40.0), 5.0));
    painter.rect_filled(track, 2.5, glass_field(palette));
    let fraction = if duration > 0 {
        (position as f32 / duration as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let fill = Rect::from_min_size(track.min, vec2(track.width() * fraction, track.height()));
    if fill.width() > 0.5 {
        painter.rect_filled(fill, 2.5, palette.accent);
    }
    if let Some(now) = now
        && now.can_control
        && duration > 0
    {
        let hit = track.expand2(vec2(10.0, 10.0));
        let response = ui.interact(hit, Id::new("glass-seek"), Sense::click_and_drag());
        if (response.dragged() || response.clicked())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let to = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
            app.actions.push(Action::Seek((to * duration as f32) as u32));
        }
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
    painter.circle_filled(
        pos2(track.left() + track.width() * fraction, track.center().y),
        5.0,
        palette.text,
    );
    painter.circle_stroke(
        pos2(track.left() + track.width() * fraction, track.center().y),
        5.0,
        Stroke::new(1.0, hairline(palette)),
    );
}

/// Shuffle, previous, play, next, repeat, and the volume. With nothing
/// playing it is a single way back to the window one picks music in.
fn transport_row(app: &mut App, ui: &mut Ui, palette: &theme::Palette, rect: Rect, now: Option<&NowPlaying>) {
    let (_, _, transport) = rows(rect);
    let Some(now) = now else {
        // The transport would be dead weight with nothing to control.
        let pill = home_pill(transport);
        let response = glass_button(ui, pill, Id::new("glass-home"), palette);
        let expand = Rect::from_center_size(
            pos2(pill.left() + 30.0, pill.center().y),
            Vec2::splat(16.0),
        );
        theme::paint_icon(ui, Icon::Expand, expand, 16.0, palette.text);
        ui.painter().text(
            pos2(expand.right() + 10.0, pill.center().y),
            Align2::LEFT_CENTER,
            gettext(app.locale, "Open the main window"),
            theme::medium(12.5),
            palette.text,
        );
        if response.clicked() {
            app.actions.push(Action::ToggleWinampWindow);
        }
        return;
    };
    let control = now.can_control;
    let playing = now.playing;
    let (shuffle, repeat) = (now.shuffle, now.repeat);

    let sizes = [30.0, 36.0, 52.0, 36.0, 30.0];
    let gap = 6.0;
    let total: f32 = sizes.iter().sum::<f32>() + gap * 4.0;
    // The cluster centres in what the volume leaves, not the whole row:
    // the speaker and its track own the right edge.
    let volume_left = transport.right() - 90.0;
    let mut x = (transport.left() + volume_left) / 2.0 - total / 2.0;
    let mut slot = |size: f32| {
        let rect = Rect::from_center_size(pos2(x + size / 2.0, transport.center().y), Vec2::splat(size));
        x += size + gap;
        rect
    };

    let shuffle_rect = slot(sizes[0]);
    let previous_rect = slot(sizes[1]);
    let play_rect = slot(sizes[2]);
    let next_rect = slot(sizes[3]);
    let repeat_rect = slot(sizes[4]);

    let shuffle_title = gettext(app.locale, "Shuffle");
    let response = icon_button(
        ui,
        shuffle_rect,
        Id::new("glass-shuffle"),
        Glyph {
            icon: Icon::Shuffle,
            size: 17.0,
            color: state_color(palette, shuffle),
        },
        control,
        palette,
    )
    .on_hover_text(shuffle_title.as_ref());
    if response.clicked() {
        app.actions.push(Action::ToggleShuffle);
    }

    let previous_title = gettext(app.locale, "Previous");
    let response = icon_button(
        ui,
        previous_rect,
        Id::new("glass-previous"),
        Glyph {
            icon: Icon::SkipBackFilled,
            size: 20.0,
            color: icon_color(palette, control),
        },
        control,
        palette,
    )
    .on_hover_text(previous_title.as_ref());
    if response.clicked() {
        app.actions.push(Action::Previous);
    }

    // The play button is the panel's own accent: a filled glass lens.
    let play_response = ui.interact(play_rect, Id::new("glass-play"), Sense::click());
    let lens = if control {
        if play_response.hovered() {
            palette.accent_hover
        } else {
            palette.accent
        }
    } else {
        glass_button_fill(palette, false)
    };
    ui.painter().circle_filled(play_rect.center(), 21.0, lens);
    ui.painter().circle_stroke(
        play_rect.center(),
        21.0,
        Stroke::new(1.0, Color32::from_white_alpha(if palette.dark { 26 } else { 120 })),
    );
    let glyph = if playing { Icon::PauseFilled } else { Icon::PlayFilled };
    let glyph_color = if control {
        Color32::WHITE
    } else {
        palette.dim
    };
    theme::paint_icon(ui, glyph, play_rect, 19.0, glyph_color);
    if play_response.clicked() && control {
        app.actions.push(Action::TogglePlay);
    }
    play_response.on_hover_cursor(if control {
        egui::CursorIcon::PointingHand
    } else {
        egui::CursorIcon::Default
    });

    let next_title = gettext(app.locale, "Next");
    let response = icon_button(
        ui,
        next_rect,
        Id::new("glass-next"),
        Glyph {
            icon: Icon::SkipForwardFilled,
            size: 20.0,
            color: icon_color(palette, control),
        },
        control,
        palette,
    )
    .on_hover_text(next_title.as_ref());
    if response.clicked() {
        app.actions.push(Action::Next);
    }

    let repeat_title = match repeat {
        RepeatMode::Off => gettext(app.locale, "Repeat"),
        RepeatMode::Track => gettext(app.locale, "Repeat one"),
        RepeatMode::Context => gettext(app.locale, "Repeat all"),
    };
    let repeat_icon = if repeat == RepeatMode::Track {
        Icon::Repeat1
    } else {
        Icon::Repeat
    };
    let response = icon_button(
        ui,
        repeat_rect,
        Id::new("glass-repeat"),
        Glyph {
            icon: repeat_icon,
            size: 17.0,
            color: state_color(palette, repeat != RepeatMode::Off),
        },
        control,
        palette,
    )
    .on_hover_text(repeat_title.as_ref());
    if response.clicked() {
        app.actions.push(Action::CycleRepeat);
    }

    volume(app, ui, palette, transport, Some(now));
}

/// The speaker and its glass track; a drag previews, a release commits.
fn volume(app: &mut App, ui: &mut Ui, palette: &theme::Palette, transport: Rect, now: Option<&NowPlaying>) {
    let level = now.map_or_else(
        || crate::app::volume_to_percent(app.local.volume),
        |now| now.volume_percent,
    );
    let shown = app.volume_preview.map_or(level, |fraction| {
        (fraction * 100.0).round() as u8
    });
    let icon = match shown {
        0 => Icon::VolumeX,
        1..=49 => Icon::Volume1,
        _ => Icon::Volume2,
    };
    let speaker = Rect::from_min_size(
        pos2(transport.right() - 64.0 - 18.0 - 8.0, transport.center().y - 9.0),
        Vec2::splat(18.0),
    );
    theme::paint_icon(ui, icon, speaker, 16.0, palette.secondary);
    let track = Rect::from_min_size(
        pos2(transport.right() - 64.0, transport.center().y - 2.5),
        vec2(64.0, 5.0),
    );
    ui.painter().rect_filled(track, 2.5, glass_field(palette));
    let fill = Rect::from_min_size(
        track.min,
        vec2(track.width() * f32::from(shown) / 100.0, track.height()),
    );
    if fill.width() > 0.5 {
        ui.painter().rect_filled(fill, 2.5, palette.accent);
    }
    let hit = track.expand2(vec2(6.0, 10.0));
    let response = ui.interact(hit, Id::new("glass-volume"), Sense::click_and_drag());
    if (response.dragged() || response.clicked() || response.drag_stopped())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let fraction = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
        if response.clicked() || response.drag_stopped() {
            app.volume_preview = None;
            app.actions
                .push(Action::SetVolume((fraction * 100.0).round() as u8));
        } else {
            app.volume_preview = Some(fraction);
            app.actions
                .push(Action::PreviewVolume((fraction * 100.0).round() as u8));
        }
    }
}

/// The equalizer: its power, the preamp, and the ten bands.
fn equalizer(app: &mut App, ui: &mut Ui, palette: &theme::Palette, rect: Rect) {
    let (_, _, transport) = rows(rect);
    let top = transport.bottom() + 14.0;
    let left = rect.left() + PAD;
    let width = rect.width() - PAD * 2.0;
    ui.painter().hline(
        (left + 2.0)..=(rect.right() - PAD - 2.0),
        top - 8.0,
        Stroke::new(1.0, hairline(palette)),
    );

    let label = gettext(app.locale, "Equalizer");
    ui.painter().text(
        pos2(left + 2.0, top + 10.0),
        Align2::LEFT_CENTER,
        label,
        theme::medium(13.0),
        palette.text,
    );

    // The power switch: a glass pill with a knob that travels.
    let switch = Rect::from_min_size(pos2(left + width - 38.0, top), vec2(38.0, 20.0));
    let on = app.settings.eq_on;
    let response = ui.interact(switch, Id::new("glass-eq-power"), Sense::click());
    ui.painter()
        .rect_filled(switch, 10.0, if on { palette.accent } else { glass_field(palette) });
    let knob_x = if on {
        switch.right() - 10.0
    } else {
        switch.left() + 10.0
    };
    ui.painter().circle_filled(
        pos2(knob_x, switch.center().y),
        8.0,
        if on { Color32::WHITE } else { palette.secondary },
    );
    if response.clicked() {
        app.actions.push(Action::ToggleEq);
    }

    let bands_top = top + 26.0;
    let range = crate::eq::RANGE_DB;
    let columns = crate::eq::BANDS.len() + 1;
    let column = width / columns as f32;
    for index in 0..columns {
        let (label, mut value) = if index == 0 {
            (
                pgettext(app.locale, "equalizer", "Pre").to_string(),
                app.settings.eq_preamp_db,
            )
        } else {
            (
                hertz(crate::eq::BANDS[index - 1]),
                app.settings.eq_bands_db[index - 1],
            )
        };
        let x = left + column * (index as f32 + 0.5);
        let area = Rect::from_min_size(
            pos2(x - column / 2.0, bands_top),
            vec2(column, BAND_HEIGHT),
        );
        let response = ui.interact(area, Id::new(("glass-eq-band", index)), Sense::click_and_drag());
        let track = Rect::from_center_size(area.center(), vec2(4.0, area.height() - 8.0));
        let y_of =
            |db: f32| track.bottom() - (db + range) / (2.0 * range) * track.height();
        let mut changed = false;
        if response.double_clicked() {
            value = 0.0;
            changed = true;
        } else if (response.dragged() || response.clicked())
            && let Some(pos) = response.interact_pointer_pos()
        {
            let db = (track.bottom() - pos.y) / track.height() * 2.0 * range - range;
            let db = (db.clamp(-range, range) * 10.0).round() / 10.0;
            if db != value {
                value = db;
                changed = true;
            }
        }
        if changed {
            if index == 0 {
                app.actions.push(Action::SetEqPreamp(value));
            } else {
                app.actions.push(Action::SetEqBand(index - 1, value));
            }
        }
        let painter = ui.painter();
        painter.rect_filled(track, 2.0, glass_field(palette));
        let fill = if on { palette.accent } else { palette.dim };
        let (fill_top, fill_bottom) = (y_of(value.max(0.0)), y_of(value.min(0.0)));
        painter.rect_filled(
            Rect::from_min_max(pos2(track.left(), fill_top), pos2(track.right(), fill_bottom)),
            2.0,
            fill,
        );
        painter.circle_filled(
            pos2(track.center().x, y_of(value)),
            6.0,
            if on { Color32::WHITE } else { palette.secondary },
        );
        if response.hovered() || response.dragged() {
            painter.text(
                pos2(area.center().x, area.top() - 2.0),
                Align2::CENTER_BOTTOM,
                format!("{value:+.1}"),
                theme::regular(10.5),
                palette.secondary,
            );
        }
        painter.text(
            pos2(area.center().x, area.bottom() + 10.0),
            Align2::CENTER_CENTER,
            label,
            theme::regular(10.0),
            palette.secondary,
        );
    }
}

/// One glyph in a glass button: what to draw and how.
struct Glyph {
    icon: Icon,
    size: f32,
    color: Color32,
}

/// One small icon button: a glass fill under the pointer, the icon on it.
fn icon_button(
    ui: &mut Ui,
    rect: Rect,
    id: Id,
    glyph: Glyph,
    enabled: bool,
    palette: &theme::Palette,
) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    if enabled && response.hovered() {
        ui.painter()
            .rect_filled(rect, 10.0, glass_button_fill(palette, true));
    }
    theme::paint_icon(ui, glyph.icon, rect, glyph.size, glyph.color);
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// A glass chip button: the fill shows under the pointer.
fn glass_button(ui: &mut Ui, rect: Rect, id: Id, palette: &theme::Palette) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    let fill = glass_button_fill(palette, response.hovered());
    ui.painter().rect_filled(rect, 12.0, fill);
    response
}

/// The fill behind a control, resting or hovered.
fn glass_button_fill(palette: &theme::Palette, hovered: bool) -> Color32 {
    if palette.dark {
        Color32::from_white_alpha(if hovered { 30 } else { 16 })
    } else {
        Color32::from_white_alpha(if hovered { 220 } else { 150 })
    }
}

/// A track or field sunk into the glass.
fn glass_field(palette: &theme::Palette) -> Color32 {
    if palette.dark {
        Color32::from_white_alpha(24)
    } else {
        Color32::from_rgba_unmultiplied(120, 140, 170, 52)
    }
}

/// The panel's hairline edge.
fn hairline(palette: &theme::Palette) -> Color32 {
    if palette.dark {
        Color32::from_white_alpha(36)
    } else {
        Color32::from_white_alpha(190)
    }
}

fn icon_color(palette: &theme::Palette, enabled: bool) -> Color32 {
    if enabled {
        palette.text
    } else {
        palette.dim
    }
}

fn state_color(palette: &theme::Palette, on: bool) -> Color32 {
    if on { palette.accent } else { palette.secondary }
}

/// `3:41` for the seek line.
fn playtime(ms: u32) -> String {
    format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60)
}

/// `60`, `1K`, `16K`: the band labels, the settings page's own words.
fn hertz(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{}K", (hz / 1000.0).round() as u32)
    } else {
        format!("{}", hz.round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_grows_with_the_equalizer() {
        let mut settings = crate::settings::Settings::default();
        let closed = window_size(&settings);
        assert_eq!(closed.x, WIDTH + MARGIN * 2.0);
        settings.eq_open = true;
        let open = window_size(&settings);
        assert_eq!(open.x, WIDTH + MARGIN * 2.0);
        assert!(open.y > closed.y, "the bands need their room");
        assert_eq!(initial_size(&settings), open);
    }

    #[test]
    fn playtime_reads_minutes_and_seconds() {
        assert_eq!(playtime(0), "0:00");
        assert_eq!(playtime(61_000), "1:01");
        assert_eq!(playtime(221_000), "3:41");
        assert_eq!(playtime(3_600_000), "60:00");
    }

    #[test]
    fn band_labels_follow_the_settings_page() {
        assert_eq!(hertz(60.0), "60");
        assert_eq!(hertz(1000.0), "1K");
        assert_eq!(hertz(16_000.0), "16K");
        assert_eq!(hertz(2400.0), "2K");
    }
}
