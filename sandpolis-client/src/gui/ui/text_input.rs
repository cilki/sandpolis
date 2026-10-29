//! Themed wrapper around Bevy's native [`EditableText`] widget.
//!
//! This is intentionally thin: it only applies the app [`Theme`] (border,
//! background, font, cursor colors) to a native [`EditableText`] node. Cursor
//! movement, text selection, IME, and clipboard paste are all handled upstream by
//! `bevy_ui_widgets::TextInputPlugin` (installed via `UiWidgetsPlugins`), whose
//! observers only act on entities carrying the [`TextInput`] marker — so the
//! bundle below always pairs it with the [`EditableText`] state.
//!
//! Features the native widget doesn't yet provide (placeholder text, password
//! masking, submit events) are deliberately omitted — we'll adopt them from
//! upstream when they land rather than maintaining our own versions.
//!
//! Read the current contents by querying [`EditableText`] and calling
//! [`EditableText::value`]; the currently focused entity is in [`InputFocus`].
//! Because focus targets the editor entity directly (and `FocusedInput` bubbles
//! *up*), attach the caller's marker component to the same entity as the editor,
//! e.g. `parent.spawn((MyInput, text_input(theme)))`.

use super::gating::WantsKeyboard;
use super::theme::{Role, Theme, ThemedBg, ThemedBorder, ThemedText};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::{EditableText, TextCursorStyle};
use bevy_ui_widgets::TextInput;

/// Move [`InputFocus`] to a text field when it's clicked.
///
/// Bevy's input dispatch doesn't focus on click by itself, so clicking a second
/// field in a form leaves focus on the first one — or nowhere. That matters
/// beyond the caret: every world hotkey is gated on
/// [`UiPointerState::wants_keyboard`](super::gating::UiPointerState), which is
/// derived from focus, so an unfocused field means the user's keystrokes fall
/// through and fire `L`/`N`/`T`/`P`/`H` instead of typing.
///
/// Registered as a global observer by [`super::UiPlugin`], so every
/// [`text_input`] gets this without opting in.
pub fn focus_text_input_on_click(
    click: On<PointerClick>,
    fields: Query<(), With<EditableText>>,
    mut focus: ResMut<InputFocus>,
) {
    if fields.contains(click.entity) && focus.get() != Some(click.entity) {
        focus.set(click.entity, FocusCause::Navigated);
    }
}

/// A themed single-line [`EditableText`] bundle.
pub fn text_input(theme: &Theme) -> impl Bundle {
    let font_size = theme.metrics.font_md;
    (
        // Lets the input gating ([`UiPointerState::wants_keyboard`]) suppress world
        // hotkeys while this field is focused.
        WantsKeyboard,
        TextInput,
        EditableText {
            allow_newlines: false,
            ..default()
        },
        TextLayout::no_wrap(),
        TextCursorStyle {
            color: theme.color(Role::Text),
            selection_color: theme.color(Role::Accent),
            ..default()
        },
        theme.text_font(font_size),
        TextColor(theme.color(Role::Text)),
        ThemedText(Role::Text),
        TabIndex(0),
        Node {
            min_width: Val::Px(220.0),
            padding: UiRect::axes(Val::Px(8.0), Val::Px(5.0)),
            border: UiRect::all(Val::Px(1.0)),
            align_items: AlignItems::Center,
            overflow: Overflow::clip(),
            // Don't let a constrained/scrolling parent squash the field.
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor(theme.color(Role::Surface)),
        ThemedBg(Role::Surface),
        BorderColor::all(theme.color(Role::Border)),
        ThemedBorder(Role::Border),
    )
}
