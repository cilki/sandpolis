//! Theming for the native `bevy_ui` GUI.
//!
//! This replaces the egui `Visuals`-based theme system. A [`Theme`] resource holds
//! a [`Palette`] of semantic colors (which swaps per [`ThemePreset`]) plus a static
//! [`Metrics`] of spacing/font sizes (which does not change between presets).
//!
//! Because `bevy_ui` is retained-mode, widgets must be (re)colored by systems
//! rather than rebuilt each frame. Tag a widget with [`ThemedBg`], [`ThemedText`],
//! or [`ThemedBorder`] and the recolor systems will paint it on spawn and repaint
//! it whenever the theme changes. Buttons use [`ThemedButton`] instead, which also
//! reflects pointer interaction (hover / pressed).

use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::Pressed;

/// Semantic color roles. Widgets reference roles, not raw colors, so a theme swap
/// recolors everything consistently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Role {
    /// App background.
    #[default]
    Background,
    /// Panel / window fill.
    Panel,
    /// Interactive surface (button/input) at rest.
    Surface,
    /// Interactive surface while hovered.
    SurfaceHover,
    /// Interactive surface while pressed / active.
    SurfaceActive,
    /// Accent / selection highlight.
    Accent,
    /// Primary text.
    Text,
    /// De-emphasized text.
    TextMuted,
    /// Borders and separators.
    Border,
    /// Warning foreground.
    Warn,
    /// Error / danger foreground.
    Error,
}

/// A full set of theme colors.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub background: Color,
    pub panel: Color,
    pub surface: Color,
    pub surface_hover: Color,
    pub surface_active: Color,
    pub accent: Color,
    pub text: Color,
    pub text_muted: Color,
    pub border: Color,
    pub warn: Color,
    pub error: Color,
}

impl Palette {
    /// Resolve a [`Role`] to its concrete color.
    pub fn color(&self, role: Role) -> Color {
        match role {
            Role::Background => self.background,
            Role::Panel => self.panel,
            Role::Surface => self.surface,
            Role::SurfaceHover => self.surface_hover,
            Role::SurfaceActive => self.surface_active,
            Role::Accent => self.accent,
            Role::Text => self.text,
            Role::TextMuted => self.text_muted,
            Role::Border => self.border,
            Role::Warn => self.warn,
            Role::Error => self.error,
        }
    }
}

/// Preset-independent layout constants (spacing, radius, font sizes).
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub space_xs: f32,
    pub space_sm: f32,
    pub space_md: f32,
    pub space_lg: f32,
    pub radius: f32,
    pub font_sm: f32,
    pub font_md: f32,
    pub font_lg: f32,
    pub font_heading: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            space_xs: 2.0,
            space_sm: 4.0,
            space_md: 8.0,
            space_lg: 16.0,
            radius: 6.0,
            font_sm: 11.0,
            font_md: 14.0,
            font_lg: 16.0,
            font_heading: 20.0,
        }
    }
}

/// Available theme presets, ported from the egui theme system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ThemePreset {
    #[default]
    SandpolisDark,
    SandpolisLight,
    Classic,
    HighContrast,
}

impl ThemePreset {
    /// Human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            ThemePreset::SandpolisDark => "Sandpolis Dark",
            ThemePreset::SandpolisLight => "Sandpolis Light",
            ThemePreset::Classic => "Classic Dark",
            ThemePreset::HighContrast => "High Contrast",
        }
    }

    /// One-line description.
    pub fn description(&self) -> &'static str {
        match self {
            ThemePreset::SandpolisDark => "Default dark theme with blue and purple accents",
            ThemePreset::SandpolisLight => "Light theme for bright environments",
            ThemePreset::Classic => "Standard dark theme",
            ThemePreset::HighContrast => "High contrast theme for accessibility",
        }
    }

    /// All presets, for building a theme picker.
    pub fn all() -> &'static [ThemePreset] {
        &[
            ThemePreset::SandpolisDark,
            ThemePreset::SandpolisLight,
            ThemePreset::Classic,
            ThemePreset::HighContrast,
        ]
    }

    /// The color palette for this preset.
    pub fn palette(&self) -> Palette {
        match self {
            ThemePreset::SandpolisDark => Palette {
                background: Color::srgb_u8(24, 26, 32),
                panel: Color::srgb_u8(32, 34, 42),
                surface: Color::srgb_u8(40, 43, 54),
                surface_hover: Color::srgb_u8(55, 60, 78),
                surface_active: Color::srgb_u8(70, 60, 96),
                accent: Color::srgb_u8(98, 114, 164),
                text: Color::srgb_u8(220, 223, 228),
                text_muted: Color::srgb_u8(150, 155, 165),
                border: Color::srgb_u8(55, 62, 84),
                warn: Color::srgb_u8(200, 160, 60),
                error: Color::srgb_u8(200, 80, 80),
            },
            ThemePreset::SandpolisLight => Palette {
                background: Color::srgb_u8(240, 242, 245),
                panel: Color::srgb_u8(250, 251, 253),
                surface: Color::srgb_u8(255, 255, 255),
                surface_hover: Color::srgb_u8(232, 238, 248),
                surface_active: Color::srgb_u8(220, 228, 246),
                accent: Color::srgb_u8(60, 90, 180),
                text: Color::srgb_u8(40, 44, 52),
                text_muted: Color::srgb_u8(110, 115, 125),
                border: Color::srgb_u8(200, 210, 230),
                warn: Color::srgb_u8(180, 140, 40),
                error: Color::srgb_u8(180, 60, 60),
            },
            ThemePreset::Classic => Palette {
                background: Color::srgb_u8(27, 27, 27),
                panel: Color::srgb_u8(37, 37, 37),
                surface: Color::srgb_u8(60, 60, 60),
                surface_hover: Color::srgb_u8(75, 75, 75),
                surface_active: Color::srgb_u8(90, 90, 90),
                accent: Color::srgb_u8(90, 140, 200),
                text: Color::srgb_u8(240, 240, 240),
                text_muted: Color::srgb_u8(160, 160, 160),
                border: Color::srgb_u8(70, 70, 70),
                warn: Color::srgb_u8(230, 200, 90),
                error: Color::srgb_u8(220, 90, 90),
            },
            ThemePreset::HighContrast => Palette {
                background: Color::BLACK,
                panel: Color::srgb_u8(20, 20, 20),
                surface: Color::srgb_u8(30, 30, 30),
                surface_hover: Color::srgb_u8(60, 60, 60),
                surface_active: Color::srgb_u8(90, 75, 0),
                accent: Color::srgb_u8(255, 200, 0),
                text: Color::WHITE,
                text_muted: Color::srgb_u8(200, 200, 200),
                border: Color::srgb_u8(255, 200, 0),
                warn: Color::srgb_u8(255, 220, 0),
                error: Color::srgb_u8(255, 60, 60),
            },
        }
    }
}

/// The active theme. Update [`Theme::set_preset`] to switch themes; the recolor
/// systems repaint all themed widgets automatically.
#[derive(Resource, Clone)]
pub struct Theme {
    pub preset: ThemePreset,
    pub palette: Palette,
    pub metrics: Metrics,
    /// Optional custom font. When `None`, Bevy's default font is used.
    pub font: Option<Handle<Font>>,
}

impl Default for Theme {
    fn default() -> Self {
        let preset = ThemePreset::default();
        Self {
            preset,
            palette: preset.palette(),
            metrics: Metrics::default(),
            font: None,
        }
    }
}

impl Theme {
    /// Resolve a [`Role`] to its concrete color in the current palette.
    pub fn color(&self, role: Role) -> Color {
        self.palette.color(role)
    }

    /// Switch to a different preset (recolors all themed widgets next frame).
    pub fn set_preset(&mut self, preset: ThemePreset) {
        self.preset = preset;
        self.palette = preset.palette();
    }

    /// A [`TextFont`] at the given size, using the theme font if one is set.
    pub fn text_font(&self, size: f32) -> TextFont {
        let mut font = TextFont::from_font_size(size);
        if let Some(handle) = &self.font {
            font.font = FontSource::Handle(handle.clone());
        }
        font
    }
}

/// Marks a node whose [`BackgroundColor`] tracks a [`Role`].
#[derive(Component, Clone, Copy, Default)]
pub struct ThemedBg(pub Role);

/// Marks a node whose [`TextColor`] tracks a [`Role`].
#[derive(Component, Clone, Copy, Default)]
pub struct ThemedText(pub Role);

/// Marks a node whose [`BorderColor`] tracks a [`Role`].
#[derive(Component, Clone, Copy, Default)]
pub struct ThemedBorder(pub Role);

/// Marks an interactive button whose background reflects the surface palette and
/// the current [`Hovered`] / [`Pressed`] state.
#[derive(Component, Clone, Copy, Default)]
pub struct ThemedButton;

/// The surface [`Role`] an interactive control shows for its pointer state:
/// pressed wins over hovered, which wins over rest.
pub fn button_surface(hovered: bool, pressed: bool) -> Role {
    if pressed {
        Role::SurfaceActive
    } else if hovered {
        Role::SurfaceHover
    } else {
        Role::Surface
    }
}

/// Installs the theme resource and recolor systems.
pub struct ThemePlugin;

impl Plugin for ThemePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Theme>().add_systems(
            Update,
            (apply_bg, apply_text, apply_border, apply_button_visuals),
        );
    }
}

/// Paint [`ThemedBg`] nodes on spawn, whenever the theme changes, and whenever
/// the node is moved to a different role (a gauge's fill does this as it fills).
fn apply_bg(theme: Res<Theme>, mut query: Query<(Ref<ThemedBg>, &mut BackgroundColor)>) {
    let theme_changed = theme.is_changed();
    for (themed, mut bg) in &mut query {
        if theme_changed || themed.is_changed() {
            bg.0 = theme.color(themed.0);
        }
    }
}

/// Paint [`ThemedText`] nodes on spawn, on a theme change, and on a role change.
fn apply_text(theme: Res<Theme>, mut query: Query<(Ref<ThemedText>, &mut TextColor)>) {
    let theme_changed = theme.is_changed();
    for (themed, mut color) in &mut query {
        if theme_changed || themed.is_changed() {
            color.0 = theme.color(themed.0);
        }
    }
}

/// Paint [`ThemedBorder`] nodes on spawn, on a theme change, and on a role change.
fn apply_border(theme: Res<Theme>, mut query: Query<(Ref<ThemedBorder>, &mut BorderColor)>) {
    let theme_changed = theme.is_changed();
    for (themed, mut border) in &mut query {
        if theme_changed || themed.is_changed() {
            *border = BorderColor::all(theme.color(themed.0));
        }
    }
}

/// Update [`ThemedButton`] backgrounds from their pointer state.
///
/// `Pressed` is a marker that `bevy_ui_widgets`' button plugin inserts and
/// removes, so there is no single component whose change ticks cover every
/// transition (a release is only visible through `RemovedComponents`). Rather
/// than pairing a change filter with a removal watcher, the wanted color is
/// recomputed each frame and written only when it differs, which is what keeps
/// the retained UI renderer from seeing a spurious change.
fn apply_button_visuals(
    theme: Res<Theme>,
    mut query: Query<(&Hovered, Has<Pressed>, &mut BackgroundColor), With<ThemedButton>>,
) {
    for (hovered, pressed, mut bg) in &mut query {
        let color = theme.color(button_surface(hovered.get(), pressed));
        bg.set_if_neq(BackgroundColor(color));
    }
}
