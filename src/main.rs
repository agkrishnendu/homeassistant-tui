use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use color_eyre::eyre::{Result, bail};
use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;

use homeassistant_tui::app::App;
use homeassistant_tui::config::Config;
use homeassistant_tui::ha::client::{self, ConnStatus, HaEvent};

/// A terminal UI for Home Assistant.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Config file (default: ~/.config/homeassistant-tui/config.toml)
    #[arg(long, short)]
    config: Option<PathBuf>,
    /// Home Assistant URL, e.g. http://homeassistant.local:8123
    #[arg(long, env = "HA_URL")]
    url: Option<String>,
    /// Long-lived access token
    #[arg(long, env = "HA_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Connect, print a summary of entities by domain, and exit
    #[arg(long)]
    dump: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref(), cli.url, cli.token)?;

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(client::run(cfg.ws_url(), cfg.token.clone(), cmd_rx, ev_tx));

    if cli.dump {
        return dump(ev_rx).await;
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(cmd_tx), ev_rx).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    mut app: App,
    mut events: mpsc::UnboundedReceiver<HaEvent>,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        terminal.draw(|f| homeassistant_tui::ui::render(f, &mut app))?;
        tokio::select! {
            ev = input.next() => match ev {
                Some(Ok(ev)) => app.on_terminal_event(ev),
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            ev = events.recv() => {
                let Some(ev) = ev else { bail!("Home Assistant client stopped unexpectedly") };
                app.on_ha_event(ev);
                // Coalesce bursts of state changes into a single redraw.
                while let Ok(ev) = events.try_recv() {
                    app.on_ha_event(ev);
                }
            }
            _ = tick.tick() => app.on_tick(),
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

async fn dump(mut events: mpsc::UnboundedReceiver<HaEvent>) -> Result<()> {
    let deadline = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => bail!("timed out waiting for Home Assistant"),
            ev = events.recv() => match ev {
                None => bail!("client stopped"),
                Some(HaEvent::Status(ConnStatus::Connected { version })) => {
                    out(format_args!("connected: Home Assistant {version}"))?;
                }
                Some(HaEvent::Status(ConnStatus::AuthFailed(m))) => bail!("authentication failed: {m}"),
                Some(HaEvent::Status(ConnStatus::Disconnected { error, .. })) => bail!("connection failed: {error}"),
                Some(HaEvent::Snapshot(s)) => {
                    let mut by_domain = std::collections::BTreeMap::<&str, usize>::new();
                    for st in &s.states {
                        *by_domain.entry(st.domain()).or_default() += 1;
                    }
                    out(format_args!(
                        "{} entities, {} areas, {} devices, {} registry entries",
                        s.states.len(),
                        s.areas.len(),
                        s.devices.len(),
                        s.entities.len()
                    ))?;
                    for (d, n) in by_domain {
                        out(format_args!("  {d:<24} {n}"))?;
                    }
                    return Ok(());
                }
                Some(_) => {}
            },
        }
    }
}

/// Print a line to stdout. A closed pipe (e.g. `--dump | head -1`) exits quietly, like other CLI tools.
fn out(line: std::fmt::Arguments) -> Result<()> {
    use std::io::Write;
    match writeln!(std::io::stdout(), "{line}") {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        r => Ok(r?),
    }
}
