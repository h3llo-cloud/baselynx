//! baselynx's two themes and the semantic colours the toolkit has no role for.
//! No colour literal appears outside this module and `assets/themes.json`.

use baselynx_core::model::ConnectionColor;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;

const THEMES: &str = include_str!("../../assets/themes.json");
pub const LIGHT: &str = "baselynx Light";
pub const DARK: &str = "baselynx Dark";

/// The base size of interface text at 100 % zoom.
pub const BASE_FONT: f32 = 13.0;
pub const ZOOM_STEPS: [f32; 11] = [0.7, 0.8, 0.9, 1.0, 1.1, 1.2, 1.35, 1.5, 1.75, 2.0, 2.5];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePref {
    System,
    Light,
    Dark,
}

impl ThemePref {
    pub fn from_saved(s: Option<&str>) -> Self {
        match s {
            Some("light") => ThemePref::Light,
            Some("dark") => ThemePref::Dark,
            _ => ThemePref::System,
        }
    }

    pub fn saved(self) -> Option<String> {
        match self {
            ThemePref::System => None,
            ThemePref::Light => Some("light".into()),
            ThemePref::Dark => Some("dark".into()),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemePref::System => "Theme follows the system",
            ThemePref::Light => "Light theme",
            ThemePref::Dark => "Dark theme",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ThemePref::System => ThemePref::Light,
            ThemePref::Light => ThemePref::Dark,
            ThemePref::Dark => ThemePref::System,
        }
    }
}

/// The user's theme choice and zoom, applied on top of the theme files.
struct Appearance {
    pref: ThemePref,
    zoom: f32,
}

impl Global for Appearance {}

/// Registers the embedded themes and applies the saved theme choice and zoom.
pub fn install(pref: ThemePref, zoom: f32, cx: &mut App) {
    ThemeRegistry::global_mut(cx)
        .load_themes_from_str(THEMES)
        .expect("embedded themes are valid");
    let registry = ThemeRegistry::global(cx);
    let light = registry.themes().get(LIGHT).cloned();
    let dark = registry.themes().get(DARK).cloned();
    if let (Some(light), Some(dark)) = (light, dark) {
        let theme = Theme::global_mut(cx);
        theme.light_theme = light;
        theme.dark_theme = dark;
    }
    cx.set_global(Appearance {
        pref,
        zoom: clamp_zoom(zoom),
    });
    apply(None, cx);
    // Background results are reported at the bottom right.
    Theme::global_mut(cx).notification.placement = Anchor::BottomRight;
}

fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() {
        z.clamp(ZOOM_STEPS[0], ZOOM_STEPS[ZOOM_STEPS.len() - 1])
    } else {
        1.0
    }
}

/// Sets the light or dark theme and the zoom. A theme change reloads the theme file, which
/// resets the font size, so the zoom is applied after it.
fn apply(window: Option<&mut Window>, cx: &mut App) {
    let pref = cx.global::<Appearance>().pref;
    let zoom = cx.global::<Appearance>().zoom;
    let mode = forced_mode().unwrap_or(match pref {
        ThemePref::Light => ThemeMode::Light,
        ThemePref::Dark => ThemeMode::Dark,
        ThemePref::System => window
            .map(|w| w.appearance())
            .unwrap_or_else(|| cx.window_appearance())
            .into(),
    });
    Theme::change(mode, None, cx);
    let mono = data_font(cx);
    Theme::update(cx, |t| {
        t.font_size = px(BASE_FONT * zoom);
        t.mono_font_size = px(BASE_FONT * zoom);
        if let Some(m) = mono {
            t.mono_font_family = m;
        }
    });
}

/// Fonts for data, best first. GPUI fails on a family it cannot load, so the
/// first installed one is chosen once.
const DATA_FONTS: [&str; 5] = ["Cascadia Mono", "SF Mono", "Menlo", "Consolas", "DejaVu Sans Mono"];

fn data_font(cx: &App) -> Option<SharedString> {
    static CHOSEN: std::sync::OnceLock<Option<SharedString>> = std::sync::OnceLock::new();
    CHOSEN
        .get_or_init(|| {
            let installed = cx.text_system().all_font_names();
            DATA_FONTS
                .iter()
                .find(|f| installed.iter().any(|n| n == *f))
                .map(|f| SharedString::from(*f))
        })
        .clone()
}

pub fn theme_pref(cx: &App) -> ThemePref {
    cx.global::<Appearance>().pref
}

pub fn set_theme_pref(pref: ThemePref, window: &mut Window, cx: &mut App) {
    cx.global_mut::<Appearance>().pref = pref;
    apply(Some(window), cx);
}

pub fn zoom(cx: &App) -> f32 {
    cx.global::<Appearance>().zoom
}

/// Moves one zoom step in or out (`delta` = +1 / -1), or back to 100 % (`0`).
pub fn step_zoom(delta: i32, cx: &mut App) -> f32 {
    let now = zoom(cx);
    let next = match delta {
        0 => 1.0,
        d if d > 0 => ZOOM_STEPS.iter().copied().find(|z| *z > now + 0.001).unwrap_or(now),
        _ => ZOOM_STEPS
            .iter()
            .rev()
            .copied()
            .find(|z| *z < now - 0.001)
            .unwrap_or(now),
    };
    cx.global_mut::<Appearance>().zoom = next;
    apply(None, cx);
    next
}

/// A size in pixels at 100 % zoom, scaled by the current zoom.
pub fn ui(n: f32, cx: &App) -> Pixels {
    px(n * zoom(cx))
}

/// `BASELYNX_THEME=light|dark` pins the theme over every other choice (for testing).
pub fn forced_mode() -> Option<ThemeMode> {
    match std::env::var("BASELYNX_THEME").ok()?.to_ascii_lowercase().as_str() {
        "light" => Some(ThemeMode::Light),
        "dark" => Some(ThemeMode::Dark),
        _ => None,
    }
}

/// Follows a system appearance change when the theme follows the system.
pub fn sync_with_system(window: &mut Window, cx: &mut App) {
    if theme_pref(cx) == ThemePref::System && forced_mode().is_none() {
        apply(Some(window), cx);
    }
}

fn dark(cx: &App) -> bool {
    cx.theme().is_dark()
}

/// Amber: a changed cell.
pub fn changed(cx: &App) -> Hsla {
    cx.theme().warning
}

pub fn changed_fill(cx: &App) -> Hsla {
    cx.theme().warning.opacity(if dark(cx) { 0.16 } else { 0.12 })
}

/// Green: a new row.
pub fn added(cx: &App) -> Hsla {
    cx.theme().success
}

pub fn added_fill(cx: &App) -> Hsla {
    cx.theme().success.opacity(if dark(cx) { 0.14 } else { 0.10 })
}

/// Red: a deleted row and errors.
pub fn removed(cx: &App) -> Hsla {
    cx.theme().danger
}

pub fn removed_fill(cx: &App) -> Hsla {
    cx.theme().danger.opacity(if dark(cx) { 0.14 } else { 0.10 })
}

/// Text colour for errors: red, readable on the window background.
pub fn error_text(cx: &App) -> Hsla {
    cx.theme().danger
}

/// Secondary text, `NULL`, `DEFAULT`, placeholders.
pub fn secondary_text(cx: &App) -> Hsla {
    cx.theme().muted_foreground
}

/// Raised surfaces: sidebar, toolbars, headers.
pub fn raised(cx: &App) -> Hsla {
    cx.theme().muted
}

pub fn connection(color: ConnectionColor, cx: &App) -> Option<Hsla> {
    match color {
        ConnectionColor::None => None,
        ConnectionColor::Green => Some(cx.theme().success),
        ConnectionColor::Amber => Some(cx.theme().warning),
        ConnectionColor::Red => Some(cx.theme().danger),
    }
}
