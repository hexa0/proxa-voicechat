use anyhow::Result;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, terminal,
};
use parking_lot::Mutex;
use proxa_client::ProxaClient;
use rand::seq::IndexedRandom;
use std::collections::VecDeque;
use std::time::Duration;
use std::{
    io::{self, Write, stdout},
    sync::LazyLock,
};

use std::sync::atomic::{AtomicBool, Ordering};

static LOG_SINK: LazyLock<Mutex<VecDeque<(log::Level, String)>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(10)));
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

// we have to do some custom log rendering to prevent funky some rendering issues in the CLI as we use terminal::EnterAlternateScreen
struct TuiLogger {
    inner: env_logger::Logger,
}
impl log::Log for TuiLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
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

static LOGGER: LazyLock<TuiLogger> = LazyLock::new(|| {
    let mut builder = env_logger::Builder::from_default_env();
    TuiLogger {
        inner: builder.build(),
    }
});

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
        long,
        help = "Frame duration in milliseconds (10, 20, 40, 60, 80)",
        default_value_t = 10.0
    )]
    frame_size: f32,

    #[arg(
        long,
        help = "Use Restricted Low Delay mode (may reduce bass for a very minimal reduced latency of ~4ms, additionally this will make FEC perform worse)"
    )]
    low_delay: bool,

    #[arg(
        short,
        long,
        help = "Number of mic channels (1 or 2)",
        default_value_t = 1
    )]
    pub channels: u8,

    #[arg(long, help = "Mute audio output (playback)")]
    pub mute_output: bool,
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
    log::set_logger(&*LOGGER).unwrap();
    log::set_max_level(LOGGER.inner.filter());
    let allow_self_signed = args.is_self_signed_allowed();

    let mut initial_room = args.room.clone();
    let mut exit_app = false;
    let mut simulated_outbound_loss: f32 = 0.0;
    let mut simulated_outbound_jitter: f32 = 0.0;
    let mut denoise_method = proxa_client::DenoiseMethod::Off;
    let mut echo_cancellation_enabled = false;

    // hook the cpal backend into proxa
    if let Err(e) = proxa_client_cpal::init() {
        log::error!("{}", e);
    }

    // test device enumeration, this didn't work since cpal has a bug with pipewire
    // on my system `pactl list sources short` and `pactl list sinks short` returns the correct results unlike cpal
    // although we don't need this i kept this code for testing in the future when cpal fixes this

    // log::info!("detected input devices:");
    // for dev in ProxaClient::enumerate_input_devices() {
    //     log::info!("  - {} (ID: {})", dev.name, dev.id);
    // }
    // log::info!("detected output devices:");
    // for dev in ProxaClient::enumerate_output_devices() {
    //     log::info!("  - {} (ID: {})", dev.name, dev.id);
    // }

    // outer loop: room selection
    loop {
        let room = if let Some(r) = initial_room.take() {
            r
        } else {
            print!("enter room name to join (or blank to exit): ");
            stdout().flush()?;
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            let r = buf.trim().to_string();
            if r.is_empty() {
                break;
            }
            r
        };

        let channel_mode = if args.channels >= 2 {
            opus::Channels::Stereo
        } else {
            opus::Channels::Mono
        };

        let client = match ProxaClient::new(proxa_client::client::ClientConfig {
            server_host: args.relay.clone(),
            room_name: room.clone(),
            channels: channel_mode,
            frame_duration_ms: args.frame_size.max(10.0),
            use_low_delay: args.low_delay,
            allow_self_signed,
        }) {
            Ok(c) => c,
            Err(e) => {
                log::error!("{}", e);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue; // retry
            }
        };

        client
            .set_bitrate(args.bitrate)
            .expect("bitrate sync failed");

        client.set_simulated_outbound_loss(simulated_outbound_loss);
        client.set_simulated_outbound_jitter(simulated_outbound_jitter);
        client.set_mute_output(args.mute_output);
        client.set_denoise_method(denoise_method);
        client.set_echo_cancellation_enabled(echo_cancellation_enabled);

        terminal::enable_raw_mode()?;
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        let mut stdout = io::stdout();
        execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // UI state that persists across ticks but not across reconnections
        let mut interval = tokio::time::interval(Duration::from_millis(8));
        let mut rng = rand::rng();
        let mut empty_room_message = NOBODY_MESSAGES.choose(&mut rng).unwrap_or(
            // this should literally never happen but we include this error for the poor soul who finds out their hardware is borked through this
            &"we've failed to perform a task that is deterministic to pass and as such cannot fail normally, your hardware is likely severly messed up as this should be impossible to trigger without faulty ram, memory corruption, or glitching your CPU",
        );
        let mut was_room_last_empty = true;

        // UI refresh loop
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let max_loss_rate = client.get_max_loss_rate();
                    let fec_active = max_loss_rate > 0.0;
                    let peers = client.get_peer_stats();
                    if peers.is_empty() && !was_room_last_empty {
                        empty_room_message = NOBODY_MESSAGES
                            .choose(&mut rng)
                            .unwrap_or(&"your hardware is severly messed up or something this should be impossible to trigger");
                    }
                    was_room_last_empty = peers.is_empty();

                    let current_bitrate = client.get_bitrate();
                    let voice_state = client.get_voice_state();


                    terminal.draw(|f| {
                        let size = f.area();

                        let controls_text = vec![
                            Line::from(vec![Span::styled("controls", Style::default().add_modifier(Modifier::BOLD))]),
                            Line::from(vec![Span::raw("x"), Span::raw(" to leave room")]),
                            Line::from(vec![Span::raw("n"), Span::raw(" to cycle denoisers")]),
                            Line::from(vec![Span::raw("e"), Span::raw(" to toggle echo cancellation")]),
                            Line::from(vec![Span::raw("m"), Span::raw(" to toggle mono/stereo")]),
                            Line::from(vec![Span::raw("up/down"), Span::raw(" arrows to adjust simulated outbound packet loss (hold shift for fine control)")]),
                            Line::from(vec![Span::raw("left/right"), Span::raw(" arrows to adjust simulated outbound jitter (hold shift for fine control)")]),
                            Line::from(""),
                        ];

                        let stats_text = vec![
                            Line::from(vec![Span::styled("stats", Style::default().add_modifier(Modifier::BOLD))]),
                            Line::from(vec![
                                Span::raw("simulated outbound packet loss: "),
                                Span::styled(format!("{:.1}%", simulated_outbound_loss * 100.0), Style::default().fg(if simulated_outbound_loss > 0.3 { Color::Red } else if simulated_outbound_loss > 0.1 { Color::Yellow } else { Color::White })),
                                Span::raw(" | jitter: "),
                                Span::styled(format!("{:.1}ms", simulated_outbound_jitter), Style::default().fg(if simulated_outbound_jitter > 100.0 { Color::Red } else if simulated_outbound_jitter > 10.0 { Color::Yellow } else { Color::White })),
                            ]),
                            Line::from(vec![
                                Span::raw("fec: "),
                                Span::styled(if fec_active { "ON" } else { "OFF" }, Style::default().fg(if fec_active { Color::Green } else { Color::Red })),
                                Span::raw(format!(" (max loss from other peers: {:.1}%)", max_loss_rate * 100.0)),
                            ]),
                            Line::from(vec![
                                Span::raw("noise reduction: "),
                                Span::styled(format!("{:?}", denoise_method), Style::default().fg(if denoise_method != proxa_client::DenoiseMethod::Off { Color::Green } else { Color::Red })),
                                Span::raw(" | h/w audio: "),
                                Span::styled(format!("{:?}", client.get_channels()), Style::default().fg(Color::Cyan)),
                                Span::raw(" | echo canceling: "),
                                Span::styled(if echo_cancellation_enabled { "ON" } else { "OFF" }, Style::default().fg(if echo_cancellation_enabled { Color::Green } else { Color::Red })),
                            ]),
                            Line::from(vec![
                                Span::raw("bitrate: "),
                                Span::styled(format!("{} bps", current_bitrate), Style::default().fg(if voice_state == proxa_client::types::VoiceState::Silenced { Color::DarkGray } else { Color::Cyan })),
                                Span::raw(" "),
                                match voice_state {
                                    proxa_client::types::VoiceState::Silenced => Span::styled("[SILENCED]", Style::default().fg(Color::DarkGray)),
                                    proxa_client::types::VoiceState::Speaking => Span::styled("[SPEAKING]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                                    proxa_client::types::VoiceState::Waiting => Span::styled("[WAITING]", Style::default().fg(Color::Yellow)),
                                }
                            ]),
                            Line::from(vec![
                                Span::raw("output: "),
                                Span::styled(if args.mute_output { "MUTED" } else { "ACTIVE" }, Style::default().fg(if args.mute_output { Color::Red } else { Color::Green })),
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
                                echo_cancellation_enabled = !echo_cancellation_enabled;
                                client.set_echo_cancellation_enabled(echo_cancellation_enabled);
                            }
                            if key.code == KeyCode::Up {
                                let delta = if key.modifiers.contains(KeyModifiers::SHIFT) { 0.01 } else { 0.10 };
                                simulated_outbound_loss = (simulated_outbound_loss + delta).min(1.0);
                                client.set_simulated_outbound_loss(simulated_outbound_loss);
                            }
                            if key.code == KeyCode::Char('m') {
                                let current = client.get_channels();
                                let next = if current == opus::Channels::Stereo {
                                    opus::Channels::Mono
                                } else {
                                    opus::Channels::Stereo
                                };
                                if let Err(e) = client.set_channels(next) {
                                    log::error!("{}", e);
                                }
                            }
                            if key.code == KeyCode::Down {
                                let delta = if key.modifiers.contains(KeyModifiers::SHIFT) { 0.01 } else { 0.10 };
                                simulated_outbound_loss = (simulated_outbound_loss - delta).max(0.0);
                                client.set_simulated_outbound_loss(simulated_outbound_loss);
                            }
                            if key.code == KeyCode::Right {
                                let delta = if key.modifiers.contains(KeyModifiers::SHIFT) { 1.0 } else { 5.0 };
                                simulated_outbound_jitter = (simulated_outbound_jitter + delta).min(1000.0);
                                client.set_simulated_outbound_jitter(simulated_outbound_jitter);
                            }
                            if key.code == KeyCode::Left {
                                let delta = if key.modifiers.contains(KeyModifiers::SHIFT) { 1.0 } else { 20.0 };
                                simulated_outbound_jitter = (simulated_outbound_jitter - delta).max(0.0);
                                client.set_simulated_outbound_jitter(simulated_outbound_jitter);
                            }
                        }
                    }
                }
                else => {
                    break;
                }
            }
        }

        // cleanup terminal state after UI loop ends
        execute!(
            terminal.backend_mut(),
            terminal::LeaveAlternateScreen,
            cursor::Show
        )?;
        terminal::disable_raw_mode()?;
        TUI_ACTIVE.store(false, Ordering::SeqCst);

        client.leave(); // explicitly leave the room

        log::info!("left room '{}'", room);

        if exit_app {
            break;
        }
    }

    Ok(())
}
