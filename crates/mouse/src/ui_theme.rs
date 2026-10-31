//! Visual theme system for the iced control panel: the six palettes of the owner's
//! redesign (R40) and the styled-widget helpers that draw it — grouped rows, the status
//! card, tiles, pill switches, segmented choices. Every colour is explicit so the panel
//! looks the same whatever the OS theme is.
//!
//! Three of the themes are *mixed*: the header and status card use a dark palette and the
//! body a light one. [`Theme6::hero`] is the top palette; for the single-tone themes it is
//! the body palette.

use iced::widget::{button, container, text_input, toggler};
use iced::{border, font, Background, Border, Color, Font, Shadow, Vector};

/// Build a `Color` from a `0xRRGGBB` literal.
const fn hex(v: u32) -> Color {
    Color::from_rgb8(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8)
}

/// The default font at semi-bold, for titles and row labels.
pub const BOLD: Font = Font { weight: font::Weight::Semibold, ..Font::DEFAULT };
/// Addresses, key caps, sizes typed into boxes.
pub const MONO: Font = Font::MONOSPACE;

/// One set of colours, named as in the design file.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub surf: Color,
    pub surf2: Color,
    pub line: Color,
    pub txt: Color,
    pub txt2: Color,
    pub txt3: Color,
    pub accent: Color,
    /// Text drawn on an accent fill.
    pub ink: Color,
    pub good: Color,
    pub warn: Color,
    pub danger: Color,
    pub dark: bool,
}

/// The six selectable themes (R40).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme6 {
    Graphite,
    Ember,
    Plum,
    Forest,
    Slate,
    Porcelain,
}

#[allow(clippy::too_many_arguments)]
const fn pal(
    bg: u32, surf: u32, surf2: u32, line: u32, txt: u32, txt2: u32, txt3: u32,
    accent: u32, ink: u32, good: u32, warn: u32, danger: u32, dark: bool,
) -> Palette {
    Palette {
        bg: hex(bg), surf: hex(surf), surf2: hex(surf2), line: hex(line),
        txt: hex(txt), txt2: hex(txt2), txt3: hex(txt3), accent: hex(accent), ink: hex(ink),
        good: hex(good), warn: hex(warn), danger: hex(danger), dark,
    }
}

impl Theme6 {
    pub const ALL: [Theme6; 6] = [
        Theme6::Graphite,
        Theme6::Ember,
        Theme6::Plum,
        Theme6::Forest,
        Theme6::Slate,
        Theme6::Porcelain,
    ];

    /// The name stored in the config.
    pub fn key(self) -> &'static str {
        match self {
            Theme6::Graphite => "graphite",
            Theme6::Ember => "ember",
            Theme6::Plum => "plum",
            Theme6::Forest => "forest",
            Theme6::Slate => "slate",
            Theme6::Porcelain => "porcelain",
        }
    }

    /// The theme a config key names; anything unknown (an older build's theme) is Amber.
    pub fn from_key(key: &str) -> Theme6 {
        Theme6::ALL.into_iter().find(|t| t.key() == key).unwrap_or(Theme6::Ember)
    }

    pub fn label(self) -> &'static str {
        match self {
            Theme6::Graphite => "Graphite",
            Theme6::Ember => "Amber",
            Theme6::Plum => "Plum & Cream",
            Theme6::Forest => "Forest & Mint",
            Theme6::Slate => "Slate & Sand",
            Theme6::Porcelain => "Porcelain",
        }
    }

    pub fn desc(self) -> &'static str {
        match self {
            Theme6::Graphite => "Dark · for developers",
            Theme6::Ember => "Dark · warm, amber accent",
            Theme6::Plum => "Mixed · dark top, cream body",
            Theme6::Forest => "Mixed · deep green top, mint body",
            Theme6::Slate => "Mixed · navy top, sand body",
            Theme6::Porcelain => "Light · soft neutral gray",
        }
    }

    /// The body palette.
    pub fn palette(self) -> Palette {
        match self {
            Theme6::Graphite => pal(
                0x16171a, 0x1e2024, 0x26282d, 0x30333a, 0xe9eaec, 0xa9adb4, 0x8a8e96,
                0x6cc4d6, 0x0c1416, 0x6fcf97, 0xe3b45c, 0xec7b72, true,
            ),
            Theme6::Ember => pal(
                0x1c1916, 0x25211d, 0x2f2a25, 0x3c352e, 0xf0ebe4, 0xbdb2a5, 0x9b8f82,
                0xe6a862, 0x1c1206, 0xa3d184, 0xe6a862, 0xec7b6e, true,
            ),
            Theme6::Plum => pal(
                0xf5f1ea, 0xfcfaf6, 0xebe5db, 0xddd5c8, 0x231a24, 0x5a4f58, 0x716670,
                0x6e3a72, 0xffffff, 0x2e7d4f, 0x8f660c, 0xb8433b, false,
            ),
            Theme6::Forest => pal(
                0xe8f0ea, 0xf7fbf8, 0xdce7df, 0xcbdacf, 0x16231c, 0x4a5a50, 0x617067,
                0x2c7553, 0xffffff, 0x2c7553, 0x8f660c, 0xb0453c, false,
            ),
            Theme6::Slate => pal(
                0xf1ece3, 0xfbf8f3, 0xe6dfd3, 0xd8cfc0, 0x1c2029, 0x525866, 0x6a7080,
                0x2c5d8a, 0xffffff, 0x2e7d4f, 0x8f660c, 0xb8433b, false,
            ),
            Theme6::Porcelain => pal(
                0xecebe8, 0xf7f7f5, 0xe2e1dd, 0xd6d4cf, 0x1b1c1e, 0x505257, 0x686a70,
                0xb86a1f, 0xffffff, 0x2e7d4f, 0x8f660c, 0xb8433b, false,
            ),
        }
    }

    /// The header and status-card palette: dark for the mixed themes, else the body's.
    pub fn hero(self) -> Palette {
        match self {
            Theme6::Plum => pal(
                0x2b1d30, 0x36263c, 0x433049, 0x4e3a55, 0xf6eef5, 0xcdbcca, 0xad9bab,
                0xe2b1df, 0x2b1d30, 0x8fd3a8, 0xe8c27a, 0xf39d93, true,
            ),
            Theme6::Forest => pal(
                0x18302a, 0x203b33, 0x29483f, 0x33544a, 0xeaf4ee, 0xb6cdbf, 0x93ad9e,
                0x8fdcb4, 0x10241d, 0x8fdcb4, 0xe8c27a, 0xf39d93, true,
            ),
            Theme6::Slate => pal(
                0x1f2836, 0x283344, 0x313e52, 0x3c4a60, 0xedf1f7, 0xb5bfce, 0x94a0b2,
                0x9cc3ec, 0x14202e, 0x8fd3a8, 0xe8c27a, 0xf39d93, true,
            ),
            other => other.palette(),
        }
    }

    /// Is the body dark? Decides the native widgets' base (scrollbars, pick lists).
    pub fn is_dark(self) -> bool {
        self.palette().dark
    }
}

/// A same-hue colour at reduced alpha.
pub fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

fn radius(r: f32) -> border::Radius {
    border::Radius::from(r)
}

fn boxed(fill: Color, line: Color, r: f32) -> container::Style {
    container::Style {
        background: Some(Background::Color(fill)),
        border: Border { color: line, width: 1.0, radius: radius(r) },
        ..container::Style::default()
    }
}

// ----- containers -----

/// A region painted in a palette's background (the window, or the hero band).
pub fn root(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.bg)),
        text_color: Some(p.txt),
        ..container::Style::default()
    }
}

/// A group of rows: one card, rows divided by hairlines.
pub fn group(p: Palette) -> container::Style {
    boxed(p.surf, p.line, 10.0)
}

/// A group drawn with an accent outline (the server's PIN box, a pinned entry).
pub fn group_accent(p: Palette) -> container::Style {
    container::Style {
        border: Border { color: p.accent, width: 1.5, radius: radius(10.0) },
        ..group(p)
    }
}

/// A paired computer's row while it is connected (R45): tinted with the "good" colour.
pub fn row_connected(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.14, ..p.good })),
        border: Border { color: Color { a: 0.55, ..p.good }, width: 1.0, radius: radius(8.0) },
        ..container::Style::default()
    }
}

/// The status card at the top of the main window.
pub fn hero(p: Palette) -> container::Style {
    boxed(p.surf, p.line, 12.0)
}

/// One computer in the status card. `active` = it has the mouse; `present` = it exists
/// (the other computer is drawn faint until it is connected).
pub fn tile(p: Palette, active: bool, present: bool) -> container::Style {
    let line = if active {
        p.accent
    } else if present {
        p.line
    } else {
        alpha(p.line, 0.7)
    };
    container::Style {
        background: Some(Background::Color(if present { p.bg } else { alpha(p.bg, 0.5) })),
        border: Border { color: line, width: if active { 1.5 } else { 1.0 }, radius: radius(10.0) },
        ..container::Style::default()
    }
}

/// A one-pixel rule.
pub fn rule(c: Color) -> container::Style {
    container::Style { background: Some(Background::Color(c)), ..container::Style::default() }
}

/// The status pill in the header.
pub fn pill(p: Palette) -> container::Style {
    boxed(p.surf, p.line, 999.0)
}

/// A small filled dot (status lights, banner markers).
pub fn dot(c: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(c)),
        border: Border { radius: radius(999.0), ..Border::default() },
        ..container::Style::default()
    }
}

/// A key cap ("Scroll Lock", "Ctrl").
pub fn keycap(p: Palette) -> container::Style {
    boxed(if p.dark { p.bg } else { p.surf2 }, p.line, 5.0)
}

/// An address chip or a kind badge ("IMG").
pub fn chip(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.surf2)),
        border: Border { radius: radius(5.0), ..Border::default() },
        ..container::Style::default()
    }
}

/// A quiet note line ("The pointer leaves this screen at…").
pub fn note(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.surf2)),
        border: Border { radius: radius(8.0), ..Border::default() },
        ..container::Style::default()
    }
}

/// The track a segmented choice sits in.
pub fn seg_track(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.surf2)),
        border: Border { radius: radius(8.0), ..Border::default() },
        ..container::Style::default()
    }
}

/// A dialog or the ⋯ menu, floating over the window.
pub fn floating(p: Palette) -> container::Style {
    container::Style {
        shadow: Shadow { color: alpha(Color::BLACK, 0.35), offset: Vector::new(0.0, 10.0), blur_radius: 30.0 },
        ..boxed(p.surf, p.line, 12.0)
    }
}

/// The area the screens picture is drawn in.
pub fn stage(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(if p.dark { alpha(Color::BLACK, 0.18) } else { p.surf2 })),
        border: Border { radius: radius(10.0), ..Border::default() },
        ..container::Style::default()
    }
}

/// A plain fill with rounded corners (theme-card previews, the edge bar).
pub fn fill(c: Color, r: f32) -> container::Style {
    container::Style {
        background: Some(Background::Color(c)),
        border: Border { radius: radius(r), ..Border::default() },
        ..container::Style::default()
    }
}

// ----- text_input -----

pub fn input(p: Palette) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style {
    move |_theme, status| {
        let focused = matches!(status, text_input::Status::Focused { .. });
        text_input::Style {
            background: Background::Color(if p.dark { p.bg } else { p.surf }),
            border: Border {
                color: if focused { p.accent } else { p.line },
                width: if focused { 1.5 } else { 1.0 },
                radius: radius(8.0),
            },
            icon: p.txt3,
            placeholder: p.txt3,
            value: p.txt,
            selection: alpha(p.accent, 0.35),
        }
    }
}

// ----- toggler -----

fn toggler_is_on(status: toggler::Status) -> bool {
    match status {
        toggler::Status::Active { is_toggled }
        | toggler::Status::Hovered { is_toggled }
        | toggler::Status::Disabled { is_toggled } => is_toggled,
    }
}

pub fn switch(p: Palette) -> impl Fn(&iced::Theme, toggler::Status) -> toggler::Style {
    move |_theme, status| {
        let on = toggler_is_on(status);
        toggler::Style {
            background: Background::Color(if on { p.accent } else { p.surf2 }),
            background_border_width: if on { 0.0 } else { 1.0 },
            background_border_color: p.line,
            foreground: Background::Color(if on { p.ink } else { p.txt3 }),
            foreground_border_width: 0.0,
            foreground_border_color: Color::TRANSPARENT,
            text_color: Some(p.txt2),
            border_radius: None,
            padding_ratio: 0.2,
        }
    }
}

// ----- buttons -----

fn btn(bg: Option<Color>, txt: Color, line: Option<Color>, r: f32) -> button::Style {
    button::Style {
        background: bg.map(Background::Color),
        text_color: txt,
        border: match line {
            Some(c) => Border { color: c, width: 1.0, radius: radius(r) },
            None => Border { radius: radius(r), ..Border::default() },
        },
        ..button::Style::default()
    }
}

/// The one strong action in a place (Start sharing, Pair, Connect).
pub fn primary(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| match status {
        button::Status::Disabled => btn(Some(p.surf2), p.txt3, None, 8.0),
        button::Status::Hovered => btn(Some(alpha(p.accent, 0.88)), p.ink, None, 8.0),
        _ => btn(Some(p.accent), p.ink, None, 8.0),
    }
}

/// An outlined everyday button (Change, Detect, Show, Later).
pub fn secondary(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| match status {
        button::Status::Disabled => btn(None, p.txt3, Some(p.line), 8.0),
        button::Status::Hovered => btn(Some(p.surf2), p.txt, Some(p.line), 8.0),
        _ => btn(Some(if p.dark { Color::TRANSPARENT } else { p.surf }), p.txt, Some(p.line), 8.0),
    }
}

/// Outlined, in the danger colour (Stop sharing, Quit anyway, Restore…).
pub fn danger(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| match status {
        button::Status::Hovered => btn(Some(alpha(p.danger, 0.10)), p.danger, Some(p.line), 8.0),
        _ => btn(Some(if p.dark { Color::TRANSPARENT } else { p.surf }), p.danger, Some(p.line), 8.0),
    }
}

/// Accent text and nothing else ("See all", "Always allow up to 6 GB").
pub fn link(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let c = if matches!(status, button::Status::Hovered) { alpha(p.accent, 0.8) } else { p.accent };
        btn(None, c, None, 6.0)
    }
}

/// A whole row that opens something; only the hover shows it is a button.
pub fn row_button(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| match status {
        button::Status::Hovered => btn(Some(alpha(p.surf2, 0.7)), p.txt, None, 0.0),
        _ => btn(None, p.txt, None, 0.0),
    }
}

/// The ⋯ button and the back arrow.
pub fn icon_button(p: Palette, outlined: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        btn(hovered.then_some(p.surf2), p.txt2, outlined.then_some(p.line), 8.0)
    }
}

/// One option of a segmented choice. The picked one is lifted out of the track.
pub fn segment(p: Palette, active: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        if active {
            btn(Some(if p.dark { p.bg } else { p.surf }), p.txt, Some(p.line), 6.0)
        } else if matches!(status, button::Status::Hovered) {
            btn(None, p.txt, None, 6.0)
        } else {
            btn(None, p.txt2, None, 6.0)
        }
    }
}

/// A tab label: no box, the underline is drawn beneath it.
pub fn tab(p: Palette, active: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let c = if active || matches!(status, button::Status::Hovered) { p.txt } else { p.txt2 };
        btn(None, c, None, 0.0)
    }
}

/// A large choice card (first-run "Shares its mouse" / "Is controlled", theme cards).
pub fn choice(p: Palette, active: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let line = if active {
            p.accent
        } else if matches!(status, button::Status::Hovered) {
            p.txt3
        } else {
            p.line
        };
        button::Style {
            background: Some(Background::Color(p.surf)),
            text_color: p.txt,
            border: Border { color: line, width: if active { 2.0 } else { 1.0 }, radius: radius(10.0) },
            ..button::Style::default()
        }
    }
}

/// A small toggle chip ("✓ Text").
pub fn chip_toggle(p: Palette, on: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, _status| {
        if on {
            btn(Some(alpha(p.accent, 0.12)), p.txt, Some(p.accent), 999.0)
        } else {
            btn(None, p.txt3, Some(p.line), 999.0)
        }
    }
}

/// The round ⇄ button between the two computers.
pub fn round_button(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        btn(Some(if hovered { p.surf2 } else { p.bg }), p.txt2, Some(p.line), 999.0)
    }
}

/// The little screen outline in front of a computer's name.
pub fn screen_glyph(c: Color) -> container::Style {
    container::Style {
        border: Border { color: c, width: 1.5, radius: radius(2.0) },
        ..container::Style::default()
    }
}

/// A radio dot on a theme card.
pub fn radio(p: Palette, on: bool) -> container::Style {
    container::Style {
        border: Border {
            color: if on { p.accent } else { p.txt3 },
            width: if on { 4.0 } else { 1.5 },
            radius: radius(999.0),
        },
        ..container::Style::default()
    }
}

/// A transfer's progress bar.
pub fn progress(p: Palette) -> impl Fn(&iced::Theme) -> iced::widget::progress_bar::Style {
    move |_theme| iced::widget::progress_bar::Style {
        background: Background::Color(p.surf2),
        bar: Background::Color(p.accent),
        border: Border { radius: radius(2.0), ..Border::default() },
    }
}

/// The dimmed layer behind a dialog.
pub fn scrim() -> container::Style {
    container::Style {
        background: Some(Background::Color(alpha(Color::BLACK, 0.55))),
        ..container::Style::default()
    }
}

/// A clickable edge strip in the screens picture: lit when it is the edge the pointer
/// leaves through.
pub fn edge_strip(p: Palette, on: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let fill = if on {
            p.accent
        } else if matches!(status, button::Status::Hovered) {
            alpha(p.accent, 0.55)
        } else {
            p.line
        };
        btn(Some(fill), p.ink, None, 3.0)
    }
}
