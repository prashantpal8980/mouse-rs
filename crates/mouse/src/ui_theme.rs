//! Visual theme system for the iced control panel: six named palettes and the
//! styled-widget helpers that render the mockup's look (rounded cards, gradient
//! accents, custom togglers/inputs/buttons). Every color is explicit so the panel
//! looks identical regardless of the OS's native theme.

use iced::widget::{button, container, text_input, toggler};
use iced::{border, Background, Border, Color, Shadow, Vector};

/// Build a `Color` from a `0xRRGGBB` literal.
const fn hex(v: u32) -> Color {
    Color::from_rgb8(((v >> 16) & 0xff) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8)
}

/// A full set of themed colors, mirroring the CSS custom properties in the mockup.
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color,
    pub bg2: Color,
    pub bg3: Color,
    pub inset: Color,
    pub line: Color,
    pub line2: Color,
    pub txt: Color,
    pub txt2: Color,
    pub txt3: Color,
    pub accent: Color,
    pub accent2: Color,
    pub accent_ink: Color,
    pub good: Color,
    pub warn: Color,
    pub danger: Color,
}

/// The six selectable themes (same set as the HTML mockup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme6 {
    Midnight,
    Oxide,
    Synth,
    Matrix,
    Nord,
    Paper,
}

impl Theme6 {
    pub const ALL: [Theme6; 6] = [
        Theme6::Midnight,
        Theme6::Oxide,
        Theme6::Synth,
        Theme6::Matrix,
        Theme6::Nord,
        Theme6::Paper,
    ];

    #[allow(dead_code)] // used for tray/tooltip labelling (wired in a later step)
    pub fn label(self) -> &'static str {
        match self {
            Theme6::Midnight => "Midnight",
            Theme6::Oxide => "Oxide",
            Theme6::Synth => "Synthwave",
            Theme6::Matrix => "Matrix",
            Theme6::Nord => "Nord",
            Theme6::Paper => "Paper",
        }
    }

    /// Is this a light-on-dark theme? (Paper is the only light one.)
    pub fn is_dark(self) -> bool {
        !matches!(self, Theme6::Paper)
    }

    pub fn palette(self) -> Palette {
        match self {
            Theme6::Midnight => Palette {
                bg: hex(0x0a0e1a), bg2: hex(0x111726), bg3: hex(0x0e1420), inset: hex(0x0b111d),
                line: hex(0x1f2839), line2: hex(0x2a3550), txt: hex(0xeef2fb), txt2: hex(0x9aa7c2),
                txt3: hex(0x5f6d8a), accent: hex(0x38d6e6), accent2: hex(0x6a7bff), accent_ink: hex(0x04121a),
                good: hex(0x37e0a0), warn: hex(0xffcf5c), danger: hex(0xff6b7a),
            },
            Theme6::Oxide => Palette {
                bg: hex(0x100b08), bg2: hex(0x1b130d), bg3: hex(0x150e09), inset: hex(0x120c07),
                line: hex(0x2c1f15), line2: hex(0x3d2c1d), txt: hex(0xf6ede3), txt2: hex(0xc2ab97),
                txt3: hex(0x836a55), accent: hex(0xff7a3d), accent2: hex(0xffb23d), accent_ink: hex(0x1a0d04),
                good: hex(0x8bd450), warn: hex(0xffc94d), danger: hex(0xff5c5c),
            },
            Theme6::Synth => Palette {
                bg: hex(0x100320), bg2: hex(0x1c0b33), bg3: hex(0x160828), inset: hex(0x12061f),
                line: hex(0x331452), line2: hex(0x4a1d75), txt: hex(0xfbe9ff), txt2: hex(0xc39cd8),
                txt3: hex(0x805a9c), accent: hex(0xff4fa3), accent2: hex(0x31d0ff), accent_ink: hex(0x1a0416),
                good: hex(0x5cffc4), warn: hex(0xffd24f), danger: hex(0xff5c8a),
            },
            Theme6::Matrix => Palette {
                bg: hex(0x030a06), bg2: hex(0x07160e), bg3: hex(0x050f09), inset: hex(0x040d07),
                line: hex(0x0e2b1a), line2: hex(0x164026), txt: hex(0xd6ffe6), txt2: hex(0x5fbf88),
                txt3: hex(0x357a52), accent: hex(0x39ff88), accent2: hex(0x7dffb0), accent_ink: hex(0x022012),
                good: hex(0x39ff88), warn: hex(0xd8ff5c), danger: hex(0xff6b5c),
            },
            Theme6::Nord => Palette {
                bg: hex(0x242a35), bg2: hex(0x2e3440), bg3: hex(0x2a303c), inset: hex(0x272d38),
                line: hex(0x3b4252), line2: hex(0x4c566a), txt: hex(0xeceff4), txt2: hex(0xa9b3c7),
                txt3: hex(0x6b7488), accent: hex(0x88c0d0), accent2: hex(0xa3be8c), accent_ink: hex(0x132029),
                good: hex(0xa3be8c), warn: hex(0xebcb8b), danger: hex(0xbf616a),
            },
            Theme6::Paper => Palette {
                bg: hex(0xeceae4), bg2: hex(0xfaf9f6), bg3: hex(0xf2f0ea), inset: hex(0xe7e4dc),
                line: hex(0xdcd8ce), line2: hex(0xc9c4b8), txt: hex(0x1d1c19), txt2: hex(0x5c584f),
                txt3: hex(0x918b7e), accent: hex(0x5b52e0), accent2: hex(0xe0527a), accent_ink: hex(0xffffff),
                good: hex(0x1f9d5b), warn: hex(0xb5820f), danger: hex(0xd13c4b),
            },
        }
    }
}

/// A same-hue color at reduced alpha (for translucent accent fills / hovers).
pub fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

/// A linear accent→accent2 gradient background, top-left to bottom-right.
pub fn accent_gradient(p: Palette) -> Background {
    Background::Gradient(
        iced::gradient::Linear::new(iced::Radians(2.2))
            .add_stop(0.0, p.accent)
            .add_stop(1.0, p.accent2)
            .into(),
    )
}

fn radius(r: f32) -> border::Radius {
    border::Radius::from(r)
}

// ----- container styles -----

/// The window background.
pub fn root(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.bg)),
        text_color: Some(p.txt),
        ..container::Style::default()
    }
}

/// A raised card panel.
pub fn card(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.bg2)),
        text_color: Some(p.txt),
        border: Border { color: p.line, width: 1.0, radius: radius(14.0) },
        shadow: Shadow { color: alpha(Color::BLACK, 0.25), offset: Vector::new(0.0, 6.0), blur_radius: 20.0 },
        ..container::Style::default()
    }
}

/// The status hero panel (accent-tinted, glowing).
pub fn hero(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.bg2)),
        text_color: Some(p.txt),
        border: Border { color: alpha(p.accent, 0.4), width: 1.0, radius: radius(16.0) },
        shadow: Shadow { color: alpha(p.accent, 0.28), offset: Vector::new(0.0, 0.0), blur_radius: 28.0 },
        ..container::Style::default()
    }
}

/// An inset/well surface (e.g. a machine "screen").
pub fn well(p: Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(p.inset)),
        text_color: Some(p.txt2),
        border: Border { color: p.line2, width: 1.0, radius: radius(12.0) },
        ..container::Style::default()
    }
}

/// A machine "screen" that is the active/engaged side (accent border + glow).
pub fn well_active(p: Palette) -> container::Style {
    container::Style {
        border: Border { color: p.accent, width: 1.5, radius: radius(12.0) },
        shadow: Shadow { color: alpha(p.accent, 0.5), offset: Vector::new(0.0, 0.0), blur_radius: 22.0 },
        ..well(p)
    }
}

/// A small pill (used for status badges); pass the fill + text colors.
pub fn pill(fill: Color, _text: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(fill)),
        border: Border { color: Color::TRANSPARENT, width: 0.0, radius: radius(999.0) },
        ..container::Style::default()
    }
}

// ----- text_input -----

pub fn input(p: Palette) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style {
    move |_theme, status| {
        let focused = matches!(status, text_input::Status::Focused { .. });
        text_input::Style {
            background: Background::Color(p.inset),
            border: Border {
                color: if focused { p.accent } else { p.line2 },
                width: if focused { 1.5 } else { 1.0 },
                radius: radius(10.0),
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
            background: if on { accent_gradient(p) } else { Background::Color(p.line2) },
            background_border_width: 0.0,
            background_border_color: Color::TRANSPARENT,
            foreground: Background::Color(Color::WHITE),
            foreground_border_width: 0.0,
            foreground_border_color: Color::TRANSPARENT,
            text_color: Some(p.txt2),
            border_radius: None,
            padding_ratio: 0.18,
        }
    }
}

// ----- buttons -----

/// The primary accent-gradient button (Start / Save-as).
pub fn primary(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let dim = matches!(status, button::Status::Disabled);
        button::Style {
            background: Some(if dim { Background::Color(p.line2) } else { accent_gradient(p) }),
            text_color: if dim { p.txt3 } else { p.accent_ink },
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: radius(11.0) },
            shadow: if dim {
                Shadow::default()
            } else {
                Shadow { color: alpha(p.accent, 0.5), offset: Vector::new(0.0, 8.0), blur_radius: 20.0 }
            },
            ..button::Style::default()
        }
    }
}

/// The red Stop/Quit button.
pub fn danger(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let dim = matches!(status, button::Status::Disabled);
        button::Style {
            background: Some(Background::Color(if dim { p.line2 } else { p.danger })),
            text_color: if dim { p.txt3 } else { Color::WHITE },
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: radius(11.0) },
            ..button::Style::default()
        }
    }
}

/// A quiet outlined button (Save / Load / Delete / Detect).
pub fn ghost(p: Palette) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        let dim = matches!(status, button::Status::Disabled);
        button::Style {
            background: Some(Background::Color(p.bg3)),
            text_color: if dim { p.txt3 } else if hovered { p.accent } else { p.txt },
            border: Border {
                color: if hovered && !dim { p.accent } else { p.line2 },
                width: 1.0,
                radius: radius(10.0),
            },
            ..button::Style::default()
        }
    }
}

/// A tab / segmented-control button. `active` = currently selected.
pub fn segment(p: Palette, active: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, _status| {
        button::Style {
            background: Some(if active { accent_gradient(p) } else { Background::Color(Color::TRANSPARENT) }),
            text_color: if active { p.accent_ink } else { p.txt2 },
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: radius(9.0) },
            ..button::Style::default()
        }
    }
}

/// A theme's colour sample, for showing beside its name (Settings → Appearance).
pub fn swatch_fill(base: Palette) -> container::Style {
    container::Style {
        background: Some(accent_gradient(base)),
        border: Border { radius: radius(6.0), ..Border::default() },
        ..container::Style::default()
    }
}

/// A clickable edge segment in the visual edge picker. `selected` fills it with accent.
pub fn edge_seg(p: Palette, selected: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered);
        button::Style {
            background: Some(if selected {
                accent_gradient(p)
            } else if hovered {
                Background::Color(alpha(p.accent, 0.5))
            } else {
                Background::Color(p.line2)
            }),
            text_color: Color::TRANSPARENT,
            border: Border { color: Color::TRANSPARENT, width: 0.0, radius: radius(6.0) },
            ..button::Style::default()
        }
    }
}
