use anyhow::{Context, Result};
use ratatui::{
    buffer::Buffer,
    crossterm::event::{Event, KeyCode, KeyEventKind},
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Widget, WidgetRef,
    },
};
use sandpolis_client::tui::{EventHandler, Panel, help::HelpWidget};
use sandpolis_instance::InstanceId;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How long to wait for the server connection before giving up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the list is re-read from the client database while it is open, so
/// agents connecting or going offline show up without restarting.
const REFRESH_PERIOD: Duration = Duration::from_secs(1);

/// One known agent, as `agents list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub instance_id: InstanceId,
    pub cluster_id: String,
    pub os: String,
    pub online: bool,
}

impl AgentRow {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "instance_id": self.instance_id.to_string(),
            "cluster_id": self.cluster_id,
            "os": self.os,
            "online": self.online,
        })
    }
}

/// Subscribe to the models [`agents`] reads. Must be called once the server
/// connection is up, or the subscription is a no-op.
pub fn subscribe() {
    sandpolis_client::sync::subscribe(sandpolis_instance::instance_manager_model_id(), None);
    sandpolis_client::sync::subscribe(
        sandpolis_instance::network::liveness::liveness_model_id(),
        None,
    );
}

/// Every agent the client's database knows about, ordered by instance id.
pub fn agents() -> Result<Vec<AgentRow>> {
    use sandpolis_instance::InstanceManagerData;
    use sandpolis_instance::network::liveness::LivenessData;
    use sandpolis_instance::realm::RealmName;

    let db = sandpolis_client::sync::client_database().context("client database unavailable")?;
    let realm = db.realm(RealmName::default())?;
    let r = realm.r_transaction()?;
    let all: Vec<InstanceManagerData> =
        r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;

    // Which of them are up is a separate, server-written model, resolved the
    // same way the GUI resolves it: an observer's word counts only while the
    // observer itself is reachable.
    let rows: Vec<LivenessData> = r.scan().primary()?.all()?.collect::<Result<Vec<_>, _>>()?;
    let online = sandpolis_instance::network::liveness::reachable(
        rows,
        sandpolis_client::sync::connected_instances(),
    );

    let mut agents: Vec<AgentRow> = all
        .into_iter()
        .filter(|i| i._instance_id.is_agent())
        .map(|i| AgentRow {
            instance_id: i._instance_id,
            cluster_id: i.cluster_id.to_string(),
            os: describe_os(&i.os_info),
            online: online.contains(&i._instance_id),
        })
        .collect();
    agents.sort_by_key(|a| a.instance_id);
    Ok(agents)
}

/// An agent's OS as one line: `os_info`'s own rendering, minus the bitness when
/// the agent couldn't determine it.
///
/// `os_info` learns the bitness on Linux by running `getconf LONG_BIT`, which
/// isn't on every PATH (a NixOS systemd service's, for one), so such an agent
/// reports `Bitness::Unknown` and the stock `Display` would tack
/// `[unknown bitness]` onto its row.
fn describe_os(info: &os_info::Info) -> String {
    let mut out = info.os_type().to_string();
    if *info.version() != os_info::Version::Unknown {
        out.push_str(&format!(" {}", info.version()));
    }
    if let Some(edition) = info.edition() {
        out.push_str(&format!(" ({edition})"));
    }
    if let Some(codename) = info.codename() {
        out.push_str(&format!(" ({codename})"));
    }
    if info.bitness() != os_info::Bitness::Unknown {
        out.push_str(&format!(" [{}]", info.bitness()));
    }
    out
}

pub struct AgentListWidget {
    state: Arc<RwLock<AgentListWidgetState>>,
    focused: bool,
}

struct AgentListWidgetState {
    status: Status,
    agents: Vec<AgentRow>,
    list_state: ListState,
    help_widget: HelpWidget,
}

enum Status {
    Connecting,
    Connected,
    Failed(String),
}

impl AgentListWidget {
    /// Opens the list and starts filling it in from the server in the
    /// background. Must be called from within a tokio runtime.
    pub fn new() -> Result<Self> {
        let state = Arc::new(RwLock::new(AgentListWidgetState {
            status: Status::Connecting,
            agents: Vec::new(),
            list_state: ListState::default(),
            help_widget: HelpWidget::new(vec![
                (KeyCode::Char('j'), "Move down".to_string()),
                (KeyCode::Char('k'), "Move up".to_string()),
            ]),
        }));

        tokio::spawn(refresh(Arc::downgrade(&state)));

        Ok(Self {
            state,
            focused: true,
        })
    }
}

/// Keep the widget's rows in step with the client database until the widget
/// is dropped.
async fn refresh(state: std::sync::Weak<RwLock<AgentListWidgetState>>) {
    if sandpolis_client::sync::wait_for_connection(CONNECT_TIMEOUT)
        .await
        .is_none()
    {
        if let Some(state) = state.upgrade() {
            state.write().unwrap().status = Status::Failed("No server connection".to_string());
        }
        return;
    }
    subscribe();

    loop {
        let rows = agents();
        {
            let Some(state) = state.upgrade() else {
                return;
            };
            let mut state = state.write().unwrap();
            match rows {
                Ok(rows) => {
                    state.status = Status::Connected;
                    state.set_agents(rows);
                }
                Err(e) => state.status = Status::Failed(format!("{e:#}")),
            }
        }
        tokio::time::sleep(REFRESH_PERIOD).await;
    }
}

impl AgentListWidgetState {
    /// Replace the rows, keeping the same agent selected if it is still there.
    fn set_agents(&mut self, agents: Vec<AgentRow>) {
        let selected = self
            .list_state
            .selected()
            .and_then(|i| self.agents.get(i))
            .map(|a| a.instance_id);
        self.agents = agents;

        let index = selected
            .and_then(|id| self.agents.iter().position(|a| a.instance_id == id))
            .or_else(|| (!self.agents.is_empty()).then_some(0));
        self.list_state.select(index);
    }
}

impl WidgetRef for AgentListWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let mut state = self.state.write().unwrap();

        let online = state.agents.iter().filter(|a| a.online).count();
        let title = match state.status {
            Status::Connected => format!("Agent List ({online}/{} online)", state.agents.len()),
            _ => "Agent List".to_string(),
        };
        let block = Block::default().borders(Borders::ALL).title(title);

        let [list_area, help_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(state.help_widget.height()),
        ])
        .areas(block.inner(area));

        block.render(area, buf);

        let message = match &state.status {
            Status::Connecting => Some(vec![
                Line::from(""),
                Line::from("Connecting to server...").gray(),
            ]),
            Status::Failed(error) => Some(vec![
                Line::from(""),
                Line::from("Cannot list agents").red(),
                Line::from(""),
                Line::from(error.clone()).gray(),
            ]),
            Status::Connected if state.agents.is_empty() => Some(vec![
                Line::from(""),
                Line::from("No agents").gray(),
                Line::from(""),
                Line::from("Agents will appear here when they connect to the server").cyan(),
            ]),
            Status::Connected => None,
        };

        if let Some(message) = message {
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .render(list_area, buf);
        } else {
            let items: Vec<ListItem> = state
                .agents
                .iter()
                .map(|agent| {
                    let status = if agent.online {
                        Span::from("online ").green()
                    } else {
                        Span::from("offline").gray()
                    };
                    ListItem::new(Line::from(vec![
                        status,
                        Span::from("  "),
                        Span::from(agent.instance_id.to_string()).blue(),
                        Span::from("  "),
                        Span::from(agent.os.clone()),
                    ]))
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Style::default().reversed())
                .highlight_symbol("> ");
            StatefulWidget::render(list, list_area, buf, &mut state.list_state);
        }

        if self.focused {
            state.help_widget.render_ref(help_area, buf);
        }
    }
}

impl EventHandler for AgentListWidget {
    fn handle_event(&mut self, event: Event) -> Option<Event> {
        let mut state = self.state.write().unwrap();

        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
        {
            state.help_widget.handle_event(event.clone())?;

            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    if !state.agents.is_empty() {
                        state.list_state.select_next();
                    }
                    return None;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !state.agents.is_empty() {
                        state.list_state.select_previous();
                    }
                    return None;
                }
                _ => {}
            }
        }

        Some(event)
    }
}

impl Panel for AgentListWidget {
    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> AgentRow {
        AgentRow {
            instance_id: id.parse().unwrap(),
            cluster_id: String::new(),
            os: String::new(),
            online: true,
        }
    }

    fn state() -> AgentListWidgetState {
        AgentListWidgetState {
            status: Status::Connected,
            agents: Vec::new(),
            list_state: ListState::default(),
            help_widget: HelpWidget::new(Vec::new()),
        }
    }

    /// A refresh must not move the cursor off the agent the user picked, even
    /// when other agents appear ahead of it.
    #[test]
    fn refresh_keeps_the_selected_agent() {
        let a = InstanceId::random(sandpolis_instance::InstanceType::Agent).to_string();
        let b = InstanceId::random(sandpolis_instance::InstanceType::Agent).to_string();
        let c = InstanceId::random(sandpolis_instance::InstanceType::Agent).to_string();

        let mut state = state();
        state.set_agents(vec![row(&b)]);
        assert_eq!(state.list_state.selected(), Some(0));

        let mut rows = vec![row(&a), row(&b), row(&c)];
        rows.sort_by_key(|r| r.instance_id);
        let expected = rows.iter().position(|r| r.instance_id.to_string() == b);
        state.set_agents(rows);
        assert_eq!(state.list_state.selected(), expected);
    }

    /// An agent that couldn't tell its bitness is described without the
    /// `[unknown bitness]` placeholder, and nothing else about the line changes.
    #[test]
    fn os_omits_unknown_bitness() {
        let info = os_info::Info::with_type(os_info::Type::NixOS);
        assert_eq!(info.bitness(), os_info::Bitness::Unknown);
        assert_eq!(info.to_string(), "NixOS [unknown bitness]");
        assert_eq!(describe_os(&info), "NixOS");

        let info = os_info::get();
        let expected = info.to_string();
        let expected = expected
            .strip_suffix(" [unknown bitness]")
            .unwrap_or(&expected);
        assert_eq!(describe_os(&info), expected);
    }

    /// The first rows to arrive select the top one; losing every row clears the
    /// selection rather than pointing past the end.
    #[test]
    fn selection_follows_the_rows() {
        let a = InstanceId::random(sandpolis_instance::InstanceType::Agent).to_string();
        let mut state = state();
        assert_eq!(state.list_state.selected(), None);

        state.set_agents(vec![row(&a)]);
        assert_eq!(state.list_state.selected(), Some(0));

        state.set_agents(Vec::new());
        assert_eq!(state.list_state.selected(), None);
    }
}
