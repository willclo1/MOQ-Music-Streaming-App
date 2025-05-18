//! CLI interface for the radio streaming application.

mod subscribe_manager;

use std::{
    io,
    sync::mpsc,
    time::{Duration, Instant},
};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CEvent, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
    widgets::{Block, Borders, List, ListItem, Paragraph, ListState, Clear},
    layout::{Layout, Constraint, Direction},
    style::{Style, Color},
};
use crate::subscribe_manager::SubscriberManager;

/// Terminal-based user interface for subscribing to audio stations.
enum InputMode {
    Normal
}

/// Holds application state for the terminal interface.
struct App {
    /// The base URL to connect to stations.
    url: String,
    /// List of available station IDs.
    stations: Vec<u16>,
    /// Index of the currently selected station.
    selected: usize,
    /// Current connection status or info message.
    status: String,
    /// Current mode of input interaction.
    input_mode: InputMode,
    /// Manages subscriber connection logic.
    mgr: SubscriberManager,
    /// Whether the user is currently connected to a station.
    is_connected: bool,
    /// Receiver for status updates from the subscriber manager.
    status_rx: mpsc::Receiver<String>,
    /// Sender used to communicate status to UI.
    status_tx: mpsc::Sender<String>,
}

impl App {
    /// Creates a new instance of the application with default state.
    ///
    /// Initializes the URL, list of stations, connection status, input mode,
    /// and communication channels for status updates.
    fn new() -> Self {
        let (status_tx, status_rx) = mpsc::channel();
        Self {
            url: "http://localhost:4443".into(),
            stations: vec![1, 2, 3],
            selected: 0,
            status: "Disconnected".into(),
            input_mode: InputMode::Normal,
            mgr: SubscriberManager::new(),
            is_connected: false,
            status_rx,
            status_tx,
        }
    }

    /// Advances selection to the next station in the list.
    ///
    /// Wraps around to the first station if currently at the last.
    fn next(&mut self) {
        self.selected = (self.selected + 1) % self.stations.len();
    }

    /// Moves selection to the previous station in the list.
    ///
    /// Wraps around to the last station if currently at the first.
    fn previous(&mut self) {
        if self.selected == 0 {
            self.selected = self.stations.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    /// Attempts to connect to the currently selected station.
    ///
    /// Updates the status message accordingly based on success or failure.
    fn connect(&mut self) {
        self.status = format!("⏱️ Connecting to station {} at {}", self.stations[self.selected], self.url);
        match self.mgr.connect(self.stations[self.selected], &self.url, self.status_tx.clone()) {
            Ok(_) => {
                self.is_connected = true;
            }
            Err(e) => {
                self.status = format!("❌ Error: {}", e);
                self.is_connected = false;
            }
        }
    }

    /// Disconnects from the current station if connected.
    ///
    /// Updates the status message accordingly based on success or failure.
    fn disconnect(&mut self) {
        match self.mgr.disconnect() {
            Ok(_) => self.status = "Disconnected".into(),
            Err(e) => self.status = format!("❌ Error: {}", e),
        }
        self.is_connected = false;
    }
}

/// Entry point for the TUI application.
/// Handles input, drawing UI components, and managing application state.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Terminal setup: enable raw mode to capture input directly,
    // switch to alternate screen buffer, and enable mouse capture.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let tick_rate = Duration::from_millis(200);
    let mut last_tick = Instant::now();
    let mut first_render = true;

    loop {
        // Flag indicating whether the UI needs to be redrawn this iteration.
        // Initially set to true on first render to draw UI immediately.
        let mut need_draw = first_render;
        first_render = false;

        // Non-blocking check for any status messages sent from the subscriber manager.
        // If a message is received, update the status and mark UI for redraw.
        if let Ok(msg) = app.status_rx.try_recv() {
            app.status = msg;
            need_draw = true;
        }

        // Calculate remaining time until next tick to use as input polling timeout.
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());

        // Poll for input events with the calculated timeout.
        // This allows the UI to remain responsive and update periodically.
        if event::poll(timeout)? {
            if let CEvent::Key(key) = event::read()? {
                match app.input_mode {
                    InputMode::Normal => match key.code {
                        // Quit application on 'q' key, disconnecting cleanly.
                        KeyCode::Char('q') => {
                            app.disconnect();
                            break;
                        }
                        // Move selection up in the station list.
                        KeyCode::Up => {
                            app.previous();
                            need_draw = true;
                        }
                        // Move selection down in the station list.
                        KeyCode::Down => {
                            app.next();
                            need_draw = true;
                        }
                        // Connect to the selected station.
                        KeyCode::Char('c') => {
                            app.disconnect();
                            app.connect();
                            need_draw = true;
                        }
                        // Disconnect from the current station.
                        KeyCode::Char('d') => {
                            app.disconnect();
                            need_draw = true;
                        }
                        _ => {}
                    },

                }
            }
        }

        // If any state changes occurred that require UI update, redraw the terminal UI.
        if need_draw {
            terminal.clear()?;
            terminal.draw(|f| {
                let area = f.size();
                // Clear the entire terminal area before drawing widgets.
                f.render_widget(Clear, area);

                // Define vertical layout with fixed height constraints for each UI section.
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints([
                        Constraint::Length(3),  // URL input display
                        Constraint::Length(5),  // Station list
                        Constraint::Length(3),  // Status message
                        Constraint::Length(3),  // Help text
                    ])
                    .split(area);

                // URL input display block showing the base URL.
                let url_block = Paragraph::new(app.url.as_ref())
                    .block(Block::default().borders(Borders::ALL).title("URL"));
                f.render_widget(url_block, chunks[0]);

                // Station list widget showing all available stations.
                // Highlights the currently selected station.
                let items: Vec<ListItem> = app
                    .stations
                    .iter()
                    .map(|s| ListItem::new(format!("Station {}", s)))
                    .collect();
                let mut state = ListState::default();
                state.select(Some(app.selected));
                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title("Stations (↑/↓)"))
                    .highlight_style(Style::default().bg(Color::Blue));
                f.render_stateful_widget(list, chunks[1], &mut state);

                // Status message block showing connection state or errors.
                let status = Paragraph::new(app.status.as_ref())
                    .block(Block::default().borders(Borders::ALL).title("Status"));
                f.render_widget(status, chunks[2]);

                // Help text block showing available key bindings.
                let help_text = "c: Connect | d: Disconnect | ↑/↓: Select Station | q: Quit";
                let help = Paragraph::new(help_text)
                    .block(Block::default().borders(Borders::ALL).title("Help"));
                f.render_widget(help, chunks[3]);
            })?;
        }
    }

    // Restore terminal to original state: disable raw mode,
    // leave alternate screen buffer, and disable mouse capture.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}