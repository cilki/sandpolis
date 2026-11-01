// TODO rename server_selection
use anyhow::Result;
use ratatui::{
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEventKind},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Borders, ListItem, Paragraph, StatefulWidget, Widget, WidgetRef},
};
use ratatui_image::{StatefulImage, protocol::StatefulProtocol};
use sandpolis_client::tui::{
    EventHandler, Panel, help::HelpWidget, loading::LoadingWidget, resident_vec::ResidentVecWidget,
};
use sandpolis_instance::database::{Data, DataCreation, DataIdentifier};
use sandpolis_server::ServerUrl;
use sandpolis_server::login::{LoginPassword, LoginRequest, LoginResponse};
use sandpolis_server::user::UserName;
use sandpolis_server::{ServerManager, client::SavedServerData};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::sleep;
use tracing::debug;
use tui_popup::{KnownSize, Popup};
use validator::Validate;

use super::GRAPHICS;

/// The logo drawn above the server list when the terminal can show images.
/// Embedded so the widget doesn't depend on any file existing at runtime.
const BANNER_PNG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../.github/images/sandpolis-256.png"
));

pub struct ServerListWidget {
    state: Arc<RwLock<ServerListWidgetState>>,
    focused: bool,
    server_manager: ServerManager,
    server_list_widget: Arc<RwLock<ResidentVecWidget<SavedServerData>>>,
}

struct ServerListWidgetState {
    default_banner_image: Option<StatefulProtocol>,
    mode: ServerListWidgetMode,
    add_server_widget: AddServerWidget,
    loading_widget: LoadingWidget,
    help_widget: HelpWidget,
}

enum ServerListWidgetMode {
    Normal,
    Adding,
    /// While we're trying to login
    TryingLogin,
    /// Successfully connected to a server
    Connected,
    Selecting,
}

impl ServerListWidget {
    pub fn new(server_manager: ServerManager) -> anyhow::Result<Self> {
        let server_list_widget = ResidentVecWidget::builder(server_manager.servers.clone())
            .title("Server Selection")
            .item_renderer(|server_resident| {
                let server_data = server_resident.read();
                let server_item = ServerListItem {
                    address: server_data.address.clone(),
                    status: RwLock::new(ServerListItemStatus::Unknown),
                    ping: RwLock::new(None),
                };
                ListItem::from(&server_item)
            })
            .event_handler(|event, list| {
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            list.select_next();
                            return None;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            list.select_previous();
                            return None;
                        }
                        _ => {}
                    }
                }
                Some(event)
            })
            .build()?;

        let state = Arc::new(RwLock::new(ServerListWidgetState {
            default_banner_image: GRAPHICS.as_ref().and_then(|picker| {
                let banner = image::load_from_memory(BANNER_PNG)
                    .inspect_err(|e| debug!(error = %e, "Failed to decode the banner image"))
                    .ok()?;
                Some(picker.new_resize_protocol(banner))
            }),
            mode: ServerListWidgetMode::Normal,
            add_server_widget: AddServerWidget::default(),
            loading_widget: LoadingWidget::new("Connecting to server..."),
            help_widget: HelpWidget::new(vec![
                (KeyCode::Char('a'), "Add a new server".to_string()),
                (KeyCode::Char('X'), "Remove server".to_string()),
                (KeyCode::Right, "Login".to_string()),
            ]),
        }));

        Ok(Self {
            state,
            focused: true,
            server_manager,
            server_list_widget: Arc::new(RwLock::new(server_list_widget)),
        })
    }
}

impl WidgetRef for ServerListWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let mut state = self.state.write().unwrap();

        let block = Block::default()
            .borders(Borders::ALL)
            .title("Server Selection");

        let [banner_area, list_area, help_area] = Layout::vertical([
            Constraint::Percentage(30),
            Constraint::Percentage(70),
            Constraint::Length(state.help_widget.height()),
        ])
        .areas(block.inner(area));

        block.render(area, buf);

        // Render banner
        if let Some(ref mut banner_image) = state.default_banner_image {
            StatefulWidget::render(StatefulImage::default(), banner_area, buf, banner_image);
        }

        // Drop the lock before rendering the list widget to avoid deadlock
        drop(state);

        // Check if the list is empty and render instructions
        let server_list_widget = self.server_list_widget.read().unwrap();
        if server_list_widget.is_empty() {
            // Render empty state instructions in the list area
            let empty_message = vec![
                Line::from(""),
                Line::from("No servers configured").gray(),
                Line::from(""),
                Line::from("Press 'a' to add a new server").cyan(),
            ];

            Paragraph::new(empty_message)
                .alignment(Alignment::Center)
                .render(list_area, buf);
        } else {
            // Render server list using ResidentVecWidget
            server_list_widget.render_ref(list_area, buf);
        }
        drop(server_list_widget);

        // Re-acquire lock for remaining rendering
        let mut state = self.state.write().unwrap();

        // Render help
        if self.focused {
            state.help_widget.render_ref(help_area, buf);
        }

        match state.mode {
            // Render dialog if we're in "add" mode
            ServerListWidgetMode::Adding => {
                let popup = Popup::new(state.add_server_widget.clone());
                // Use the full buffer area to center the popup on the entire screen
                Widget::render(popup, buf.area, buf);
            }
            // Render loading dialog during login
            ServerListWidgetMode::TryingLogin => {
                state.loading_widget.update_animation();
                let popup = Popup::new(state.loading_widget.clone());
                // Use the full buffer area to center the popup on the entire screen
                Widget::render(popup, buf.area, buf);
            }
            // Show connected state
            ServerListWidgetMode::Connected => {
                // Display a simple message that we're connected
                let message = vec![
                    Line::from(""),
                    Line::from("Successfully connected!").green(),
                    Line::from(""),
                    Line::from("Agent management coming soon...").gray(),
                    Line::from(""),
                    Line::from("Press Esc to return").cyan(),
                ];

                Paragraph::new(message)
                    .alignment(Alignment::Center)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("Connected")
                            .style(Style::default().white()),
                    )
                    .render(
                        ratatui::layout::Layout::default()
                            .direction(ratatui::layout::Direction::Vertical)
                            .constraints([
                                ratatui::layout::Constraint::Percentage(30),
                                ratatui::layout::Constraint::Percentage(40),
                                ratatui::layout::Constraint::Percentage(30),
                            ])
                            .split(buf.area)[1],
                        buf,
                    );
            }
            _ => (),
        }
    }
}

enum ServerListItemStatus {
    Unknown,
    Loading,
    LoginFailed,
    ConnectionFailed,
    /// Some other kind of failure
    Failed,
    Ok,
}

struct ServerListItem {
    address: ServerUrl,
    status: RwLock<ServerListItemStatus>,
    ping: RwLock<Option<u32>>,
}

impl ServerListItem {
    async fn run(&self) {
        loop {
            match *self.status.read().unwrap() {
                ServerListItemStatus::Loading => {}
                ServerListItemStatus::Ok => {
                    sleep(Duration::from_secs(3)).await;
                    // TODO ping
                }
                ServerListItemStatus::ConnectionFailed => {
                    sleep(Duration::from_secs(3)).await;
                    // TODO try again
                }
                _ => {}
            }
        }
    }
}

impl From<&ServerListItem> for ListItem<'_> {
    fn from(item: &ServerListItem) -> Self {
        let mut text = Text::default();

        // Add server name
        text.extend([format!("{}", item.address).blue()]);

        // Add online status
        match &*item.status.read().unwrap() {
            ServerListItemStatus::Loading => text.extend(["Loading".to_string().gray()]),
            ServerListItemStatus::Ok => text.extend(["Online".to_string().green()]),
            ServerListItemStatus::ConnectionFailed => text.extend(["Offline".to_string()]),
            ServerListItemStatus::Failed => text.extend(["Error".to_string().red()]),
            ServerListItemStatus::Unknown => text.extend(["Unknown".to_string().gray()]),
            ServerListItemStatus::LoginFailed => text.extend(["Login failed".to_string().red()]),
        }
        ListItem::new(text)
    }
}

impl EventHandler for ServerListWidget {
    fn handle_event(&mut self, event: Event) -> Option<Event> {
        // Let help widget handle event first (it never consumes)
        let mut state = self.state.write().unwrap();

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            match state.mode {
                ServerListWidgetMode::Normal => {
                    state.help_widget.handle_event(event.clone())?;

                    match key.code {
                        KeyCode::Char('a') => {
                            state.mode = ServerListWidgetMode::Adding;
                            debug!("Entering add server mode");
                            return None;
                        }
                        KeyCode::Char('X') => {
                            // Remove the currently selected server
                            drop(state);
                            let server_list_widget = self.server_list_widget.read().unwrap();
                            if let Some(selected_server) = server_list_widget.selected() {
                                let server_id = selected_server.read().id();
                                drop(server_list_widget);

                                debug!(server_id = ?server_id, "Removing server");
                                if let Err(e) = self.server_manager.remove_server(server_id) {
                                    debug!(error = %e, "Failed to remove server");
                                }
                            }
                            return None;
                        }
                        KeyCode::Right => {
                            // Login to the currently selected server
                            let server_list_widget = self.server_list_widget.read().unwrap();
                            if let Some(selected_server) = server_list_widget.selected() {
                                let server_data = selected_server.read().clone();
                                drop(server_list_widget);
                                state.mode = ServerListWidgetMode::TryingLogin;
                                drop(state);

                                let server_manager = self.server_manager.clone();
                                let state_clone = self.state.clone();

                                debug!(address = %server_data.address, "Connecting to server");

                                tokio::spawn(async move {
                                    match server_manager.connect(server_data.address.clone()).await
                                    {
                                        Ok(connection) => {
                                            // Requests on this connection carry
                                            // the saved token from the last login.
                                            if server_data.token.is_usable() {
                                                *connection.token.write().unwrap() =
                                                    Some(server_data.token.clone());
                                            }
                                            debug!("Connected successfully");
                                            state_clone.write().unwrap().mode =
                                                ServerListWidgetMode::Connected;
                                        }
                                        Err(e) => {
                                            debug!(error = %e, "Failed to connect to server");
                                            state_clone.write().unwrap().mode =
                                                ServerListWidgetMode::Normal;
                                        }
                                    }
                                });
                            }
                            return None;
                        }
                        _ => {
                            // Delegate navigation events to the ResidentVecWidget
                            drop(state);
                            if let Some(unhandled_event) =
                                self.server_list_widget.write().unwrap().handle_event(event)
                            {
                                return Some(unhandled_event);
                            }
                            return None;
                        }
                    }
                }
                ServerListWidgetMode::TryingLogin => {
                    if key.code == KeyCode::Esc {
                        state.mode = ServerListWidgetMode::Normal;
                        state.add_server_widget = AddServerWidget::default(); // Reset form
                        debug!("Exiting add server mode");
                        return None;
                    }
                }
                ServerListWidgetMode::Adding => match key.code {
                    KeyCode::Esc => {
                        state.mode = ServerListWidgetMode::Normal;
                        state.add_server_widget = AddServerWidget::default(); // Reset form
                        debug!("Exiting add server mode");
                        return None;
                    }
                    KeyCode::Tab => {
                        state.add_server_widget.next_field();
                        return None;
                    }
                    KeyCode::BackTab => {
                        state.add_server_widget.prev_field();
                        return None;
                    }
                    KeyCode::Enter => {
                        if let Ok(form_data) = state.add_server_widget.get_form_data() {
                            state.mode = ServerListWidgetMode::TryingLogin;

                            let server_manager = self.server_manager.clone();
                            let state = self.state.clone();

                            tokio::spawn(async move {
                                state.write().unwrap().add_server_widget =
                                    AddServerWidget::default(); // Reset login form

                                match server_manager.connect(form_data.server_url.clone()).await {
                                    Ok(connection) => {
                                        match connection
                                            .login(LoginRequest {
                                                username: form_data.username.clone(),
                                                password: LoginPassword::new(
                                                    connection.cluster_id,
                                                    &form_data.password,
                                                ),
                                                setup: false,
                                                totp_token: form_data.totp,
                                                lifetime: None,
                                            })
                                            .await
                                        {
                                            Ok(LoginResponse::Ok(client_auth_token)) => {
                                                debug!("Login successful, saving server");
                                                match server_manager.save_server(SavedServerData {
                                                        address: form_data.server_url,
                                                        token: client_auth_token,
                                                        user: form_data.username,
                                                        // TODO this sucks
                                                        _id: DataIdentifier::default(),
                                                        _revision:
                                                            sandpolis_instance::database::DataRevision::Latest(
                                                                0,
                                                            ),
                                                        _creation: DataCreation::default(),
                                                    }) {
                                                        Ok(_) => {
                                                            debug!("Server saved successfully");
                                                            state.write().unwrap().mode =
                                                                ServerListWidgetMode::Normal;
                                                        }
                                                        Err(e) => {
                                                            debug!(
                                                                error = %e,
                                                                "Failed to save server"
                                                            );
                                                            // TODO show error dialog
                                                            state.write().unwrap().mode =
                                                                ServerListWidgetMode::Normal;
                                                        }
                                                    }
                                            }
                                            _ => {
                                                debug!("Login failed");
                                                // TODO show login failed dialog
                                                state.write().unwrap().mode =
                                                    ServerListWidgetMode::Normal;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        debug!(error = %e, "Connection failed");
                                        // TODO show connection failed dialog
                                        state.write().unwrap().mode = ServerListWidgetMode::Normal;
                                    }
                                }
                            });
                        }
                        return None;
                    }
                    KeyCode::Backspace => {
                        state.add_server_widget.handle_backspace();
                        return None;
                    }
                    KeyCode::Delete => {
                        state.add_server_widget.handle_delete();
                        return None;
                    }
                    KeyCode::Left => {
                        state.add_server_widget.move_cursor_left();
                        return None;
                    }
                    KeyCode::Right => {
                        state.add_server_widget.move_cursor_right();
                        return None;
                    }
                    KeyCode::Char(ch) => {
                        state.add_server_widget.handle_char_input(ch);
                        return None;
                    }
                    _ => {}
                },
                // Neither mode owns any keys of its own yet, so anything
                // arriving here falls through to whoever owns this widget.
                ServerListWidgetMode::Connected | ServerListWidgetMode::Selecting => {}
            }
        }
        Some(event)
    }
}

impl Panel for ServerListWidget {
    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
        self.server_list_widget.write().unwrap().set_focus(focused);
    }
}

#[derive(Debug, Clone, Default)]
struct AddServerWidget {
    server_url: String,
    username: String,
    password: String,
    totp: String,
    focused_field: FormField,
    cursor_position: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum FormField {
    #[default]
    ServerUrl,
    Username,
    Password,
    Totp,
}

impl AddServerWidget {
    fn next_field(&mut self) {
        self.focused_field = match self.focused_field {
            FormField::ServerUrl => FormField::Username,
            FormField::Username => FormField::Password,
            FormField::Password => FormField::Totp,
            FormField::Totp => FormField::ServerUrl,
        };
        self.update_cursor_position();
    }

    fn prev_field(&mut self) {
        self.focused_field = match self.focused_field {
            FormField::ServerUrl => FormField::Totp,
            FormField::Username => FormField::ServerUrl,
            FormField::Password => FormField::Username,
            FormField::Totp => FormField::Password,
        };
        self.update_cursor_position();
    }

    fn update_cursor_position(&mut self) {
        self.cursor_position = match self.focused_field {
            FormField::ServerUrl => self.server_url.len(),
            FormField::Username => self.username.len(),
            FormField::Password => self.password.len(),
            FormField::Totp => self.totp.len(),
        };
    }

    fn current_field_value(&self) -> &str {
        match self.focused_field {
            FormField::ServerUrl => &self.server_url,
            FormField::Username => &self.username,
            FormField::Password => &self.password,
            FormField::Totp => &self.totp,
        }
    }

    fn current_field_value_mut(&mut self) -> &mut String {
        match self.focused_field {
            FormField::ServerUrl => &mut self.server_url,
            FormField::Username => &mut self.username,
            FormField::Password => &mut self.password,
            FormField::Totp => &mut self.totp,
        }
    }

    fn handle_char_input(&mut self, ch: char) {
        // Apply input filtering for TOTP field
        if self.focused_field == FormField::Totp {
            // Only allow numeric characters
            if !ch.is_ascii_digit() {
                return;
            }
            // Limit to 6 characters
            if self.totp.len() >= 6 {
                return;
            }
        }

        let cursor_pos = self.cursor_position;
        let field = self.current_field_value_mut();
        field.insert(cursor_pos, ch);
        self.cursor_position += 1;
    }

    fn handle_backspace(&mut self) {
        if self.cursor_position > 0 {
            let cursor_pos = self.cursor_position;
            let field = self.current_field_value_mut();
            field.remove(cursor_pos - 1);
            self.cursor_position -= 1;
        }
    }

    fn handle_delete(&mut self) {
        let cursor_pos = self.cursor_position;
        let field = self.current_field_value_mut();
        if cursor_pos < field.len() {
            field.remove(cursor_pos);
        }
    }

    fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
        }
    }

    fn move_cursor_right(&mut self) {
        let field_len = self.current_field_value().len();
        if self.cursor_position < field_len {
            self.cursor_position += 1;
        }
    }

    fn validate_totp(&self) -> Result<(), String> {
        let totp_str = self.totp.trim();
        if totp_str.is_empty() {
            return Ok(()); // TOTP is optional
        }

        if totp_str.len() != 6 {
            return Err("TOTP must be exactly 6 digits".to_string());
        }

        if !totp_str.chars().all(|c| c.is_ascii_digit()) {
            return Err("TOTP must contain only numeric characters".to_string());
        }

        Ok(())
    }

    fn get_validation_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if self.server_url.trim().is_empty() {
            errors.push("Server URL is required".to_string());
        } else if let Err(_err) = self.server_url.parse::<ServerUrl>() {
            errors.push("Invalid server URL".to_string());
        }

        if self.username.parse::<UserName>().is_err() {
            errors.push("Username is required".to_string());
        }

        if self.password.trim().is_empty() {
            errors.push("Password is required".to_string());
        }

        if let Err(err) = self.validate_totp() {
            errors.push(err);
        }

        errors
    }

    fn get_form_data(&self) -> Result<ServerFormData> {
        let data = ServerFormData {
            server_url: self.server_url.parse()?,
            username: self.username.parse()?,
            password: self.password.clone(),
            totp: if self.totp.is_empty() {
                None
            } else {
                Some(self.totp.to_string())
            },
        };
        data.validate()?;

        Ok(data)
    }
}

#[derive(Debug, Clone, Validate)]
struct ServerFormData {
    password: String,
    server_url: ServerUrl,
    totp: Option<String>,
    username: UserName,
}

impl KnownSize for AddServerWidget {
    fn width(&self) -> usize {
        60
    }

    fn height(&self) -> usize {
        12
    }
}

impl Widget for AddServerWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.render_ref(area, buf);
    }
}

impl WidgetRef for AddServerWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title("Add Server")
            .borders(Borders::ALL)
            .style(Style::default().white());

        let inner = block.inner(area);
        block.render(area, buf);

        // Layout: title + 4 form fields + submit/cancel instructions
        let chunks = Layout::vertical([
            Constraint::Length(2), // Server URL
            Constraint::Length(2), // Username
            Constraint::Length(2), // Password
            Constraint::Length(2), // TOTP (optional)
            Constraint::Length(1), // Spacing
            Constraint::Length(1), // Instructions
        ])
        .split(inner);

        // Render Server URL field
        self.render_field(
            "Server URL",
            &self.server_url,
            FormField::ServerUrl,
            chunks[0],
            buf,
        );

        // Render Username field
        self.render_field(
            "Username",
            &self.username,
            FormField::Username,
            chunks[1],
            buf,
        );

        // Render Password field
        self.render_password_field(chunks[2], buf);

        // Render TOTP field
        self.render_field(
            "TOTP (6 digits)",
            &self.totp,
            FormField::Totp,
            chunks[3],
            buf,
        );

        // Render instructions or validation errors
        let errors = self.get_validation_errors();
        let text = if self.get_form_data().is_ok() {
            Line::from("Enter: Connect  •  Esc: Cancel  •  Tab: Next Field").gray()
        } else if !errors.is_empty() {
            Line::from(format!("Errors: {}", errors.join(", "))).red()
        } else {
            Line::from("Tab: Next Field  •  Esc: Cancel  •  (Enter server URL, username, and password to connect)").gray()
        };

        Paragraph::new(text).render(chunks[5], buf);
    }
}

impl AddServerWidget {
    fn render_field(
        &self,
        label: &str,
        value: &str,
        field_type: FormField,
        area: Rect,
        buf: &mut Buffer,
    ) {
        let is_focused = self.focused_field == field_type;
        let style = if is_focused {
            Style::default().white().on_blue()
        } else {
            Style::default().gray()
        };

        // Create display value with cursor if focused
        let display_value = if is_focused {
            let mut chars: Vec<char> = value.chars().collect();
            if self.cursor_position <= chars.len() {
                chars.insert(self.cursor_position, '│');
            }
            chars.into_iter().collect()
        } else {
            value.to_string()
        };

        let chunks = Layout::horizontal([
            Constraint::Length(label.len() as u16 + 2),
            Constraint::Min(1),
        ])
        .split(area);

        // Render label
        Paragraph::new(format!("{}: ", label))
            .style(Style::default().white())
            .render(chunks[0], buf);

        // Render input field
        Paragraph::new(display_value)
            .style(style)
            .render(chunks[1], buf);
    }

    fn render_password_field(&self, area: Rect, buf: &mut Buffer) {
        let is_focused = self.focused_field == FormField::Password;
        let style = if is_focused {
            Style::default().white().on_blue()
        } else {
            Style::default().gray()
        };

        // Mask password with asterisks
        let masked_password = if is_focused {
            let mut masked: Vec<char> = self.password.chars().map(|_| '*').collect();
            if self.cursor_position <= masked.len() {
                masked.insert(self.cursor_position, '│');
            }
            masked.into_iter().collect()
        } else {
            "*".repeat(self.password.len())
        };

        let chunks = Layout::horizontal([
            Constraint::Length(11), // "Password: ".len()
            Constraint::Min(1),
        ])
        .split(area);

        // Render label
        Paragraph::new("Password: ")
            .style(Style::default().white())
            .render(chunks[0], buf);

        // Render masked input field
        Paragraph::new(masked_password)
            .style(style)
            .render(chunks[1], buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_is_a_decodable_image() {
        let banner = image::load_from_memory(BANNER_PNG).unwrap();
        assert!(banner.width() > 0 && banner.height() > 0);
    }
}
