use anyhow::Result;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, terminal,
};
use parking_lot::Mutex;
use proxa_client::ProxaClient;
use proxa_client_cpal::start_audio_backend;
use rand::seq::IndexedRandom;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use std::{
    io::{self, Write, stdout},
    sync::LazyLock,
};

use std::sync::atomic::{AtomicBool, Ordering};

static LOG_SINK: LazyLock<Mutex<VecDeque<(log::Level, String)>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(10)));
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

// we have to do some custom log rendering to prevent funky some rendering issues
struct TuiLogger;
impl log::Log for TuiLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        if TUI_ACTIVE.load(Ordering::Relaxed) {
            let mut sink = LOG_SINK.lock();
            if sink.len() >= sink.capacity() {
                sink.pop_front();
            }
            sink.push_back((record.level(), format!("{}", record.args())));
        } else {
            use crossterm::style::{Color as CColor, ResetColor, SetForegroundColor};
            let (level_str, color) = match record.level() {
                log::Level::Error => ("ERROR", CColor::Red),
                log::Level::Warn => ("WARN", CColor::Yellow),
                log::Level::Info => ("INFO", CColor::Cyan),
                log::Level::Debug => ("DEBUG", CColor::Grey),
                log::Level::Trace => ("TRACE", CColor::Grey),
            };
            eprintln!(
                "{}[{}]{} {}",
                SetForegroundColor(color),
                level_str,
                ResetColor,
                record.args()
            );
        }
    }
    fn flush(&self) {}
}
static LOGGER: TuiLogger = TuiLogger;

use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(
        short,
        long,
        help = "domain/ipv4/[ipv6]:port of the relay server",
        default_value = "127.0.0.1:39201"
    )]
    relay: String,

    #[arg(
        long,
        help = "Allow self-signed certificates (default in debug builds)"
    )]
    allow_self_signed: bool,

    #[arg(
        long,
        help = "Disallow self-signed certificates (default in release builds)"
    )]
    disallow_self_signed: bool,

    #[arg(help = "Room name to join automatically")]
    room: Option<String>,

    #[arg(
        short,
        long,
        help = "Audio bitrate (e.g. 24000, 32000, 64000, 128000)",
        default_value_t = 32000
    )]
    bitrate: i32,

    #[arg(
        short,
        long,
        help = "Number of audio channels (1 for Mono, 2 for Stereo)",
        default_value_t = 1
    )]
    channels: u8,
}

impl Args {
    fn is_self_signed_allowed(&self) -> bool {
        if self.disallow_self_signed {
            false
        } else if self.allow_self_signed {
            true
        } else {
            cfg!(debug_assertions)
        }
    }
}

/// random message to display in the CLI when nobody is connected to a vc room
static NOBODY_MESSAGES: LazyLock<Vec<&str>> = LazyLock::new(|| {
    vec![
        "nobody else is here",
        "but nobody came",
        "anybody there..?",
        "looks like nobody's in the room.. you should give someone the room code me thinks",
        "looks like you're wasting precious UDP packets for nothing, instead of feeling ashamed of all that wasted bandwidth maybe invite someone to the room?",
        "anybody home?",
        "howdy anybody there?..... oh no-one is there :(",
        "maybe there'd be some peers here if you invited some ya dingus!",
        "anybody got a peer?", // this wasn't even intentional i just didn't finish the sentence LMFAO, accidental free joke i guess
        "any peers around here?",
        "this town wasn't big enough for the one of us.. but it could be more if you invited someone",
        "...",
        "lol look who doesn't have friends to voice chat with",
        "it seems as though your peers aren't home and as such are too busy to connect",
    ]
});

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(log::LevelFilter::Info);
    let allow_self_signed = args.is_self_signed_allowed();

    let mut initial_room = args.room.clone();
    let mut exit_app = false;
    let mut simulated_loss: f32 = 0.0;
    let mut denoise_method = proxa_client::DenoiseMethod::Off;
    let mut aec_enabled = false;

    // keep audio drivers warm from startup
    let client_slot = Arc::new(parking_lot::Mutex::new(None));
    let _audio_backend = match start_audio_backend(client_slot.clone()) {
        Ok(b) => Some(b),
        Err(e) => {
            log::warn!(
                "Failed to initialize audio drivers: {}. You may not hear anything.",
                e
            );
            None
        }
    };

    // outer loop: room selection
    loop {
        let room = if let Some(r) = initial_room.take() {
            r
        } else {
            print!("Enter room name to join (or blank to exit): ");
            stdout().flush()?;
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            let r = buf.trim().to_string();
            if r.is_empty() {
                break;
            }
            r
        };

        // inner loop: connection and UI logic (with auto-reconnect)
        loop {
            let channel_mode = if args.channels >= 2 {
                opus::Channels::Stereo
            } else {
                opus::Channels::Mono
            };

            log::info!(
                "connecting to {} room '{}' with {} channels at {} bps...",
                args.relay,
                room,
                args.channels,
                args.bitrate
            );

            let client =
                match ProxaClient::connect(&args.relay, &room, channel_mode, allow_self_signed)
                    .await
                {
                    Ok(c) => Arc::new(c),
                    Err(e) => {
                        log::error!("failed to connect: {}. retrying in 2 seconds...", e);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue; // retry
                    }
                };
            log::info!("client object created");

            client
                .set_bitrate(args.bitrate)
                .expect("failed to change bitrate");

            client.set_simulated_loss(simulated_loss);
            client.set_denoise_method(denoise_method);
            client.set_aec(aec_enabled);

            // load DeepFilterNet3 models if present (handled by client crate)
            client.auto_load_models();

            // plug client into active audio stream
            *client_slot.lock() = Some(client.clone());

            terminal::enable_raw_mode()?;
            TUI_ACTIVE.store(true, Ordering::SeqCst);
            let mut stdout = io::stdout();
            execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
            let backend = CrosstermBackend::new(stdout);
            let mut terminal = Terminal::new(backend)?;

            // UI state that persists across ticks but not across reconnections
            let mut interval = tokio::time::interval(Duration::from_millis(8));
            let mut exit_room = false;
            let mut rng = rand::rng();
            let mut empty_room_message = NOBODY_MESSAGES
                .choose(&mut rng)
                .unwrap_or(&"your hardware is severly messed up or something this should be impossible to trigger");
            let mut was_empty = true;

            // UI refresh loop
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let max_loss_rate = client.get_max_loss_rate();
                        let fec_active = max_loss_rate > 0.0;
                        let peers = client.get_peer_stats();
                        if peers.is_empty() && !was_empty {
                            empty_room_message = NOBODY_MESSAGES
                                .choose(&mut rng)
                                .unwrap_or(&"your hardware is severly messed up or something this should be impossible to trigger");
                        }
                        was_empty = peers.is_empty();

                        let local_volume = client.get_local_stats();
                        let current_bitrate = client.get_bitrate();
                        let is_silent = client.is_silent();


                        terminal.draw(|f| {
                            let size = f.area();

                            let controls_text = vec![
                                Line::from(vec![Span::styled("controls", Style::default().add_modifier(Modifier::BOLD))]),
                                Line::from(vec![Span::raw("x"), Span::raw(" to leave room")]),
                                Line::from(vec![Span::raw("n"), Span::raw(" to cycle denoise")]),
                                Line::from(vec![Span::raw("e"), Span::raw(" to toggle echo cancellation")]),
                                Line::from(vec![Span::raw("up/down"), Span::raw(" arrows to adjust simulated outbound packet loss (hold shift for fine control)")]),
                                Line::from(""),
                            ];

                            let stats_text = vec![
                                Line::from(vec![Span::styled("stats", Style::default().add_modifier(Modifier::BOLD))]),
                                Line::from(vec![
                                    Span::raw("simulated outbound packet loss: "),
                                    Span::styled(format!("{:.1}%", simulated_loss * 100.0), Style::default().fg(if simulated_loss > 0.3 { Color::Red } else if simulated_loss > 0.1 { Color::Yellow } else { Color::White })),
                                ]),
                                Line::from(vec![
                                    Span::raw("fec: "),
                                    Span::styled(if fec_active { "ON" } else { "OFF" }, Style::default().fg(if fec_active { Color::Green } else { Color::Red })),
                                    Span::raw(format!(" (max loss from other peers: {:.1}%)", max_loss_rate * 100.0)),
                                ]),
                                Line::from(vec![
                                    Span::raw("noise reduction: "),
                                    Span::styled(format!("{:?}", denoise_method), Style::default().fg(if denoise_method != proxa_client::DenoiseMethod::Off { Color::Green } else { Color::Red })),
                                    Span::raw(" | echo cancel: "),
                                    Span::styled(if aec_enabled { "ON" } else { "OFF" }, Style::default().fg(if aec_enabled { Color::Green } else { Color::Red })),
                                ]),
                                Line::from(vec![
                                    Span::raw("bitrate: "),
                                    Span::styled(format!("{} bps", current_bitrate), Style::default().fg(if is_silent { Color::DarkGray } else { Color::Cyan })),
                                    Span::raw(" "),
                                    if is_silent {
                                        Span::styled("[SILENCED]", Style::default().fg(Color::DarkGray))
                                    } else if local_volume > proxa_client::VOICE_THRESHOLD {
                                        Span::styled("[SPEAKING]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
                                    } else {
                                        Span::styled("[WAITING]", Style::default().fg(Color::Yellow))
                                    }
                                ]),
                                Line::from(""),
                            ];

                            let log_sink = LOG_SINK.lock();
                            let logs_text: Vec<Line> = log_sink.iter().map(|(level, msg)| {
                                let (level_str, level_color) = match level {
                                    log::Level::Error => ("ERROR", Color::Red),
                                    log::Level::Warn => ("WARN", Color::Yellow),
                                    log::Level::Info => ("INFO", Color::Cyan),
                                    log::Level::Debug => ("DEBUG", Color::DarkGray),
                                    log::Level::Trace => ("TRACE", Color::DarkGray),
                                };
                                Line::from(vec![
                                    Span::styled(format!("[{}] ", level_str), Style::default().fg(level_color)),
                                    Span::raw(msg)
                                ])
                            }).collect();

                            let chunks = Layout::default()
                                .direction(Direction::Vertical)
                                .constraints([
                                    Constraint::Length(controls_text.len() as u16),
                                    Constraint::Length(stats_text.len() as u16),
                                    Constraint::Min(5),
                                    Constraint::Length(logs_text.len() as u16),
                                ])
                                .split(size);

                            let controls = Paragraph::new(controls_text)
                                .block(Block::default().borders(Borders::NONE));
                            f.render_widget(controls, chunks[0]);

                            let stats = Paragraph::new(stats_text)
                                .block(Block::default().borders(Borders::NONE));
                            f.render_widget(stats, chunks[1]);

                            let mut peers_items = vec![];
                            peers_items.push(ListItem::new(Line::from(vec![
                                Span::styled(format!("room: '{}'", room), Style::default().add_modifier(Modifier::BOLD))
                            ])));

                            if peers.is_empty() {
                                peers_items.push(ListItem::new(Line::from(vec![
                                    Span::styled(format!("  {}", empty_room_message), Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC))
                                ])));
                            } else {
                                for (id, volume, target_jitter) in &peers {
                                    let indicator = if *volume > proxa_client::VOICE_THRESHOLD {
                                        Span::styled("(*) ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                                    } else {
                                        Span::raw("( ) ")
                                    };

                                    let volume_color = if *volume > 0.5 { Color::Red } else if *volume > 0.1 { Color::Yellow } else { Color::Gray };

                                    peers_items.push(ListItem::new(Line::from(vec![
                                        indicator,
                                        Span::styled(format!("peer id {}", id), Style::default().add_modifier(Modifier::BOLD)),
                                        Span::raw(" (peak volume: "),
                                        Span::styled(format!("{:.3}", volume), Style::default().fg(volume_color)),
                                        Span::raw(") [jitter buffer: "),
                                        Span::styled(format!("{} frames", target_jitter), Style::default().fg(Color::Yellow)),
                                        Span::raw("]"),
                                    ])));
                                }
                            }

                            let peers_list = List::new(peers_items)
                                .block(Block::default().borders(Borders::NONE));
                            f.render_widget(peers_list, chunks[2]);

                            if !logs_text.is_empty() {
                                let logs = Paragraph::new(logs_text)
                                    .block(Block::default().borders(Borders::NONE));
                                f.render_widget(logs, chunks[3]);
                            }
                        })?;

                        if event::poll(Duration::from_millis(0))? {
                            if let Event::Key(key) = event::read()? {
                                if key.code == KeyCode::Char('x') {
                                    exit_room = true;
                                    break; // break UI loop
                                }
                                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                                    exit_app = true;
                                    break; // break UI loop
                                }
                                if key.code == KeyCode::Char('n') {
                                    denoise_method = denoise_method.next();
                                    client.set_denoise_method(denoise_method);
                                }
                                if key.code == KeyCode::Char('e') {
                                    aec_enabled = !aec_enabled;
                                    client.set_aec(aec_enabled);
                                }
                                if key.code == KeyCode::Up {
                                    let delta = if key.modifiers.contains(KeyModifiers::SHIFT) { 0.01 } else { 0.10 };
                                    simulated_loss = (simulated_loss + delta).min(1.0);
                                    client.set_simulated_loss(simulated_loss);
                                }
                                if key.code == KeyCode::Down {
                                    let delta = if key.modifiers.contains(KeyModifiers::SHIFT) { 0.01 } else { 0.10 };
                                    simulated_loss = (simulated_loss - delta).max(0.0);
                                    client.set_simulated_loss(simulated_loss);
                                }
                            }
                        }
                    }
                    _ = client.connection.closed() => {
                        break; // server closed connection
                    }
                }
            }

            // unplug client from audio stream
            *client_slot.lock() = None;

            // cleanup terminal state after UI loop ends
            execute!(
                terminal.backend_mut(),
                terminal::LeaveAlternateScreen,
                cursor::Show
            )?;
            terminal::disable_raw_mode()?;
            TUI_ACTIVE.store(false, Ordering::SeqCst);

            client.leave(); // explicitly leave the room

            if exit_room || exit_app {
                break; // break inner connection loop if user intentionally left or exited
            }

            log::error!("connection lost. Attempting to reconnect to '{}'...", room);
            tokio::time::sleep(Duration::from_secs(2)).await; // wait before retrying connection
        }

        log::info!("left room '{}'", room);

        if exit_app {
            break;
        }
    }

    Ok(())
}
