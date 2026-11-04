//! tmux-web binary: the web server (owned by the server unit).

use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;
use tmux_web::config::{Cli, Config, LogFormat};
use tmux_web::server;
use tracing_subscriber::EnvFilter;

/// How long the runtime may take to wind down after the server stopped.
const RUNTIME_WIND_DOWN: Duration = Duration::from_secs(1);

fn main() -> ExitCode {
    let cli = match Cli::try_parse_args(std::env::args_os()) {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };
    init_logging(cli.global.log_format);
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("tmux-web: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Logs go to stderr at `RUST_LOG` (default `info`); JSON unless stderr is a terminal.
fn init_logging(format: LogFormat) {
    let stderr_is_terminal = std::io::stderr().is_terminal();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    let result = if format.is_json(stderr_is_terminal) {
        builder.json().try_init()
    } else {
        builder.with_ansi(stderr_is_terminal).try_init()
    };
    if let Err(err) = result {
        eprintln!("tmux-web: could not set up logging: {err}");
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let config = Config::from_cli(cli)?;
    serve(config)
}

fn serve(config: Config) -> Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("tmux-web")
        .build()
        .context("starting the async runtime")?;
    let result = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("binding {}", config.listen))?;
        server::run(config, listener, shutdown_signal()).await
    });
    // Blocking tmux calls still in flight must not hold the process up.
    runtime.shutdown_timeout(RUNTIME_WIND_DOWN);
    result.map(|()| ExitCode::SUCCESS)
}

/// Completes on the first SIGINT or SIGTERM; a second one ends the process at once.
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (mut interrupt, mut terminate) = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
        (Err(err), _) | (_, Err(err)) => {
            tracing::error!(%err, "could not install signal handlers; the server can only be killed");
            std::future::pending::<()>().await;
            return;
        }
    };
    tokio::select! {
        _ = interrupt.recv() => tracing::info!("received SIGINT; shutting down"),
        _ = terminate.recv() => tracing::info!("received SIGTERM; shutting down"),
    }
    tokio::spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        eprintln!("tmux-web: second signal; exiting now");
        std::process::exit(130);
    });
}
