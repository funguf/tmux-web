//! CLI options and runtime configuration (owned by the server unit).
//!
//! [`Cli`] is the clap definition; [`Config`] is the resolved runtime configuration built from
//! it (paths made absolute, defaults applied, authentication decided).

use crate::tmux::{self, SocketSel, TmuxConfig};
use crate::util;
use anyhow::{anyhow, bail, Context, Result};
use clap::parser::ValueSource;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::fmt;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7690";
/// Session created when tmux is started from the page with no server running (`--default-session`).
pub const DEFAULT_SESSION_NAME: &str = "main";
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 1500;
pub const DEFAULT_MAX_UPLOAD_MB: u64 = 25;
pub const DEFAULT_UPLOAD_RETENTION_DAYS: u64 = 7;

/// File name of the generated auth token inside the state directory.
pub const TOKEN_FILE: &str = "token";

/// Smallest accepted `--poll-interval-ms`; anything lower would spin on `tmux list-sessions`.
const MIN_POLL_INTERVAL_MS: u64 = 10;

const SECS_PER_DAY: u64 = 86_400;

/// How log lines are written to stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    /// JSON when stderr is not a terminal, human readable text otherwise.
    Auto,
    Text,
    Json,
}

impl LogFormat {
    /// True when log lines should be JSON, given whether stderr is a terminal.
    pub fn is_json(self, stderr_is_terminal: bool) -> bool {
        match self {
            LogFormat::Json => true,
            LogFormat::Text => false,
            LogFormat::Auto => !stderr_is_terminal,
        }
    }
}

/// Options shared by every subcommand. They may be given before or after the subcommand.
#[derive(Args, Clone, Debug)]
pub struct GlobalOpts {
    /// tmux binary to run
    #[arg(
        long,
        global = true,
        env = "TMUX_WEB_TMUX_BIN",
        value_name = "PATH",
        default_value = "tmux"
    )]
    pub tmux_bin: PathBuf,

    /// tmux server socket name (`tmux -L`); default is tmux's default server
    #[arg(
        short = 'L',
        long,
        global = true,
        env = "TMUX_WEB_SOCKET_NAME",
        value_name = "NAME",
        conflicts_with = "tmux_socket_path"
    )]
    pub tmux_socket_name: Option<String>,

    /// tmux server socket path (`tmux -S`)
    #[arg(
        short = 'S',
        long,
        global = true,
        env = "TMUX_WEB_SOCKET_PATH",
        value_name = "PATH"
    )]
    pub tmux_socket_path: Option<PathBuf>,

    /// State directory [default: $XDG_STATE_HOME/tmux-web, else ~/.local/state/tmux-web]
    #[arg(long, global = true, env = "TMUX_WEB_STATE_DIR", value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    /// Claude Code config directory [default: ~/.claude]
    #[arg(long, global = true, env = "CLAUDE_CONFIG_DIR", value_name = "DIR")]
    pub claude_config_dir: Option<PathBuf>,

    /// Log format on stderr
    #[arg(
        long,
        global = true,
        env = "TMUX_WEB_LOG_FORMAT",
        value_enum,
        default_value_t = LogFormat::Auto
    )]
    pub log_format: LogFormat,
}

/// Options of the `serve` subcommand (also accepted without the subcommand).
#[derive(Args, Clone)]
pub struct ServeOpts {
    /// Address to listen on
    #[arg(
        long,
        env = "TMUX_WEB_LISTEN",
        value_name = "ADDR:PORT",
        default_value = DEFAULT_LISTEN
    )]
    pub listen: String,

    /// Require this token (Bearer header, `tmux_web_token` cookie or `?token=`)
    #[arg(
        long,
        env = "TMUX_WEB_TOKEN",
        value_name = "TOKEN",
        hide_env_values = true
    )]
    pub token: Option<String>,

    /// Serve without authentication (required to listen on a non-loopback address without a token)
    #[arg(long)]
    pub no_auth: bool,

    /// Additional Host header / Origin host to accept (repeatable, comma separated in the env var)
    #[arg(
        long = "allow-host",
        env = "TMUX_WEB_ALLOW_HOSTS",
        value_name = "HOST",
        value_delimiter = ','
    )]
    pub allow_host: Vec<String>,

    /// How often to poll tmux for sessions, in milliseconds
    #[arg(long, value_name = "N", default_value_t = DEFAULT_POLL_INTERVAL_MS)]
    pub poll_interval_ms: u64,

    /// Session tmux-web creates when it starts tmux with no server running
    #[arg(
        long,
        env = "TMUX_WEB_DEFAULT_SESSION",
        value_name = "NAME",
        default_value = DEFAULT_SESSION_NAME
    )]
    pub default_session: String,

    /// Largest accepted image upload, in MiB
    #[arg(long, value_name = "N", default_value_t = DEFAULT_MAX_UPLOAD_MB)]
    pub max_upload_mb: u64,

    /// Delete uploaded images older than this many days
    #[arg(long, value_name = "N", default_value_t = DEFAULT_UPLOAD_RETENTION_DAYS)]
    pub upload_retention_days: u64,
}

impl fmt::Debug for ServeOpts {
    /// Never prints the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServeOpts")
            .field("listen", &self.listen)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("no_auth", &self.no_auth)
            .field("allow_host", &self.allow_host)
            .field("poll_interval_ms", &self.poll_interval_ms)
            .field("default_session", &self.default_session)
            .field("max_upload_mb", &self.max_upload_mb)
            .field("upload_retention_days", &self.upload_retention_days)
            .finish()
    }
}

#[derive(Subcommand, Clone, Debug)]
pub enum Cmd {
    /// Run the web server (the default)
    Serve(ServeOpts),
}

/// `tmux-web [GLOBAL] [serve] [SERVE]`; `serve` is the default subcommand.
#[derive(Parser, Clone, Debug)]
#[command(
    name = "tmux-web",
    version,
    about = "Browser panels for your tmux sessions",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    /// Serve options, used when no subcommand is given
    #[command(flatten)]
    pub serve: ServeOpts,

    #[command(subcommand)]
    pub command: Option<Cmd>,
}

impl Cli {
    /// Parses `args` (including the program name). Serve options given at the top level
    /// together with a subcommand are an error: they would be silently ignored otherwise.
    pub fn try_parse_args<I, T>(args: I) -> Result<Cli, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let mut command = Cli::command();
        let matches = command.try_get_matches_from_mut(args)?;
        let cli = Cli::from_arg_matches(&matches)?;
        if cli.command.is_some() {
            let top_level_serve_option = ServeOpts::augment_args(clap::Command::new("serve"))
                .get_arguments()
                .any(|arg| {
                    matches.value_source(arg.get_id().as_str()) == Some(ValueSource::CommandLine)
                });
            if top_level_serve_option {
                return Err(command.error(
                    clap::error::ErrorKind::ArgumentConflict,
                    "serve options must be given after the `serve` subcommand",
                ));
            }
        }
        Ok(cli)
    }

    /// The effective serve options: the `serve` subcommand's, or the top-level ones.
    pub fn serve_opts(&self) -> &ServeOpts {
        match &self.command {
            None => &self.serve,
            Some(Cmd::Serve(opts)) => opts,
        }
    }
}

/// How requests are authenticated.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthSetup {
    /// Anyone who can reach the socket (the Host check still applies).
    Disabled,
    /// A request needs this token.
    Token { token: String, source: TokenSource },
}

/// Where the active token came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenSource {
    /// `--token` / `TMUX_WEB_TOKEN`.
    Explicit,
    /// Read from `<state-dir>/token`.
    File,
    /// Generated and written to `<state-dir>/token` by this run.
    Generated,
}

impl AuthSetup {
    pub fn token(&self) -> Option<&str> {
        match self {
            AuthSetup::Disabled => None,
            AuthSetup::Token { token, .. } => Some(token),
        }
    }
}

impl fmt::Debug for AuthSetup {
    /// Never prints the token.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthSetup::Disabled => f.write_str("Disabled"),
            AuthSetup::Token { source, .. } => f
                .debug_struct("Token")
                .field("token", &"<redacted>")
                .field("source", source)
                .finish(),
        }
    }
}

/// Resolved runtime configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub tmux: TmuxConfig,
    /// Absolute path of the state directory.
    pub state_dir: PathBuf,
    pub claude_config_dir: PathBuf,
    pub log_format: LogFormat,
    pub listen: SocketAddr,
    pub auth: AuthSetup,
    /// Lower-case `--allow-host` entries.
    pub allow_hosts: Vec<String>,
    pub poll_interval: Duration,
    /// Valid tmux session name created when tmux is started from the page with no server running.
    pub default_session: String,
    pub max_upload_bytes: u64,
    pub upload_retention: Duration,
}

impl Config {
    /// Configuration from the global options alone: serve-only settings take their
    /// defaults and authentication stays disabled. Touches no files.
    pub fn from_global(global: &GlobalOpts) -> Result<Config> {
        let socket = match (
            non_empty_str(global.tmux_socket_name.as_deref()),
            non_empty_path(global.tmux_socket_path.as_deref()),
        ) {
            (Some(_), Some(_)) => {
                bail!("-L/--tmux-socket-name and -S/--tmux-socket-path cannot be combined")
            }
            (Some(name), None) => SocketSel::Name(name.to_string()),
            (None, Some(path)) => SocketSel::Path(path.to_path_buf()),
            (None, None) => SocketSel::Default,
        };
        let state_dir = match non_empty_path(global.state_dir.as_deref()) {
            Some(dir) => dir.to_path_buf(),
            None => default_state_dir(std::env::var_os("XDG_STATE_HOME"), &util::home_dir()),
        };
        let claude_config_dir = match non_empty_path(global.claude_config_dir.as_deref()) {
            Some(dir) => dir.to_path_buf(),
            None => util::home_dir().join(".claude"),
        };
        Ok(Config {
            tmux: TmuxConfig {
                bin: global.tmux_bin.clone(),
                socket,
            },
            state_dir: std::path::absolute(&state_dir)
                .with_context(|| format!("resolving state directory {}", state_dir.display()))?,
            claude_config_dir: std::path::absolute(&claude_config_dir).with_context(|| {
                format!(
                    "resolving Claude config directory {}",
                    claude_config_dir.display()
                )
            })?,
            log_format: global.log_format,
            listen: DEFAULT_LISTEN
                .parse()
                .map_err(|e| anyhow!("default listen address: {e}"))?,
            auth: AuthSetup::Disabled,
            allow_hosts: Vec::new(),
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
            default_session: DEFAULT_SESSION_NAME.to_string(),
            max_upload_bytes: DEFAULT_MAX_UPLOAD_MB * 1024 * 1024,
            upload_retention: Duration::from_secs(DEFAULT_UPLOAD_RETENTION_DAYS * SECS_PER_DAY),
        })
    }

    /// Configuration for `serve`. Reads or creates `<state-dir>/token` when authentication
    /// needs a generated token.
    pub fn for_serve(global: &GlobalOpts, serve: &ServeOpts) -> Result<Config> {
        let mut config = Config::from_global(global)?;
        config.listen = parse_listen(&serve.listen)?;
        config.allow_hosts = serve
            .allow_host
            .iter()
            .map(|host| host.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect();
        if serve.poll_interval_ms < MIN_POLL_INTERVAL_MS {
            bail!("--poll-interval-ms must be at least {MIN_POLL_INTERVAL_MS}");
        }
        if serve.max_upload_mb == 0 {
            bail!("--max-upload-mb must be at least 1");
        }
        if serve.upload_retention_days == 0 {
            bail!("--upload-retention-days must be at least 1");
        }
        if let Err(problem) = tmux::validate_session_name(&serve.default_session) {
            bail!(
                "--default-session {:?} is not a valid session name: {problem}",
                serve.default_session
            );
        }
        config.poll_interval = Duration::from_millis(serve.poll_interval_ms);
        config.default_session = serve.default_session.clone();
        config.max_upload_bytes = serve
            .max_upload_mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow!("--max-upload-mb is too large"))?;
        config.upload_retention = Duration::from_secs(
            serve
                .upload_retention_days
                .checked_mul(SECS_PER_DAY)
                .ok_or_else(|| anyhow!("--upload-retention-days is too large"))?,
        );
        config.auth = match plan_auth(
            &config.listen,
            non_empty_str(serve.token.as_deref()),
            serve.no_auth,
        )? {
            AuthPlan::Disabled => AuthSetup::Disabled,
            AuthPlan::Explicit(token) => AuthSetup::Token {
                token,
                source: TokenSource::Explicit,
            },
            AuthPlan::StateFile => {
                util::ensure_private_dir(&config.state_dir).with_context(|| {
                    format!("creating state directory {}", config.state_dir.display())
                })?;
                let (token, generated) = load_or_create_token(&config.state_dir)?;
                AuthSetup::Token {
                    token,
                    source: if generated {
                        TokenSource::Generated
                    } else {
                        TokenSource::File
                    },
                }
            }
        };
        Ok(config)
    }

    /// Configuration for what `cli` asks for.
    pub fn from_cli(cli: &Cli) -> Result<Config> {
        Config::for_serve(&cli.global, cli.serve_opts())
    }

    /// Directory holding uploaded images.
    pub fn uploads_dir(&self) -> PathBuf {
        self.state_dir.join("uploads")
    }
}

fn non_empty_str(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

fn non_empty_path(value: Option<&Path>) -> Option<&Path> {
    value.filter(|v| !v.as_os_str().is_empty())
}

/// `$XDG_STATE_HOME/tmux-web` when that variable is set to an absolute path (the XDG rule),
/// else `<home>/.local/state/tmux-web`.
pub fn default_state_dir(xdg_state_home: Option<OsString>, home: &Path) -> PathBuf {
    match xdg_state_home.as_deref().map(Path::new) {
        Some(dir) if dir.is_absolute() => dir.join("tmux-web"),
        _ => home.join(".local").join("state").join("tmux-web"),
    }
}

/// Parses `ADDR:PORT`; host names such as `localhost:7690` are resolved.
pub fn parse_listen(listen: &str) -> Result<SocketAddr> {
    if let Ok(addr) = listen.parse::<SocketAddr>() {
        return Ok(addr);
    }
    listen
        .to_socket_addrs()
        .with_context(|| format!("invalid --listen address {listen:?} (expected ADDR:PORT)"))?
        .next()
        .ok_or_else(|| anyhow!("--listen address {listen:?} did not resolve to any address"))
}

/// True for 127.0.0.0/8, ::1 and IPv4-mapped loopback addresses.
pub fn is_loopback(addr: &SocketAddr) -> bool {
    addr.ip().to_canonical().is_loopback()
}

/// What the authentication options ask for, before any file is touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthPlan {
    Disabled,
    Explicit(String),
    /// Read or generate `<state-dir>/token`.
    StateFile,
}

/// Decides authentication from the listen address, `--token` and `--no-auth`:
///
/// * a token is always honoured, and `--no-auth` together with a token is an error;
/// * a loopback address needs no auth unless a token is given;
/// * a non-loopback address is never served unauthenticated unless `--no-auth` says so:
///   without a token it gets the one in `<state-dir>/token`.
pub fn plan_auth(listen: &SocketAddr, token: Option<&str>, no_auth: bool) -> Result<AuthPlan> {
    match (token, no_auth) {
        (Some(_), true) => bail!("--token and --no-auth cannot be combined"),
        (Some(token), false) => {
            validate_token(token)?;
            Ok(AuthPlan::Explicit(token.to_string()))
        }
        (None, true) => Ok(AuthPlan::Disabled),
        (None, false) if is_loopback(listen) => Ok(AuthPlan::Disabled),
        (None, false) => Ok(AuthPlan::StateFile),
    }
}

/// A token travels in URLs, cookies and headers, so it is restricted to URL-unreserved
/// characters: no escaping rules can ever differ between those places.
pub fn validate_token(token: &str) -> Result<()> {
    if token.is_empty() {
        bail!("the auth token must not be empty");
    }
    if !token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    {
        bail!("the auth token may only contain letters, digits, '-', '.', '_' and '~'");
    }
    Ok(())
}

/// Reads `<state-dir>/token`, or creates it with 32 random bytes as hex (mode 0600).
/// Returns the token and whether it was newly generated.
pub fn load_or_create_token(state_dir: &Path) -> Result<(String, bool)> {
    let path = state_dir.join(TOKEN_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let token = content.trim();
            if validate_token(token).is_ok() {
                tighten_token_mode(&path);
                return Ok((token.to_string(), false));
            }
            tracing::warn!(path = %path.display(), "ignoring invalid auth token file; generating a new token");
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", path.display()));
        }
    }
    let token = util::random_hex(32);
    util::atomic_write(&path, token.as_bytes(), 0o600)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok((token, true))
}

/// The token file may have been created by hand with a laxer mode.
fn tighten_token_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!(path = %path.display(), %err, "could not restrict the auth token file to mode 0600");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};
    use std::os::unix::fs::PermissionsExt;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_args(std::iter::once("tmux-web").chain(args.iter().copied()))
    }

    /// The configuration of `tmux-web serve <args>`, with every file kept in a temporary state
    /// directory.
    fn serve_config(args: &[&str]) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let mut full = vec!["--state-dir", state.to_str().unwrap(), "serve"];
        full.extend_from_slice(args);
        Config::from_cli(&parse(&full).unwrap())
    }

    #[test]
    fn serve_is_the_default_subcommand_and_serve_options_work_without_it() {
        let cli = parse(&["--listen", "127.0.0.1:0", "--poll-interval-ms", "50"]).unwrap();
        assert!(cli.command.is_none());
        let serve = cli.serve_opts();
        assert_eq!(serve.listen, "127.0.0.1:0");
        assert_eq!(serve.poll_interval_ms, 50);
    }

    #[test]
    fn global_options_may_precede_the_serve_subcommand() {
        let cli = parse(&["-L", "tmw-srv-smoke", "serve", "--listen", "127.0.0.1:0"]).unwrap();
        assert_eq!(
            cli.global.tmux_socket_name.as_deref(),
            Some("tmw-srv-smoke")
        );
        assert_eq!(cli.serve_opts().listen, "127.0.0.1:0");
    }

    #[test]
    fn global_options_may_follow_the_subcommand_too() {
        let cli = parse(&["serve", "-L", "x"]).unwrap();
        assert_eq!(cli.global.tmux_socket_name.as_deref(), Some("x"));
        assert!(matches!(cli.command, Some(Cmd::Serve(_))));
    }

    #[test]
    fn serve_options_before_the_subcommand_are_rejected() {
        let err = parse(&["--no-auth", "serve"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn socket_name_and_path_conflict() {
        assert!(parse(&["-L", "a", "-S", "/tmp/x"]).is_err());
    }

    #[test]
    fn the_snapshot_and_restore_subcommands_are_gone() {
        for command in ["snapshot", "snapshots", "restore"] {
            assert!(parse(&[command]).is_err(), "{command}");
        }
        for flag in [
            "--auto-restore",
            "--auto-restore-on-start",
            "--snapshot-interval-secs=3",
            "--offer-max-age-days=2",
            "--claude-cmd=claude",
        ] {
            assert!(parse(&[flag]).is_err(), "{flag}");
        }
    }

    #[test]
    fn allow_host_is_repeatable_and_comma_separated() {
        let cli = parse(&[
            "serve",
            "--allow-host",
            "a.example",
            "--allow-host",
            "b.example,c.example",
        ])
        .unwrap();
        assert_eq!(
            cli.serve_opts().allow_host,
            ["a.example", "b.example", "c.example"]
        );
    }

    #[test]
    fn debug_output_never_contains_the_token() {
        let cli = parse(&["serve", "--token", "super-secret-token"]).unwrap();
        assert!(!format!("{cli:?}").contains("super-secret-token"));
        let setup = AuthSetup::Token {
            token: "super-secret-token".into(),
            source: TokenSource::Explicit,
        };
        assert!(!format!("{setup:?}").contains("super-secret-token"));
    }

    #[test]
    fn default_state_dir_follows_xdg_then_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            default_state_dir(Some("/xdg/state".into()), home),
            PathBuf::from("/xdg/state/tmux-web")
        );
        // A relative XDG_STATE_HOME must be ignored, as the XDG spec says.
        assert_eq!(
            default_state_dir(Some("relative".into()), home),
            PathBuf::from("/home/u/.local/state/tmux-web")
        );
        assert_eq!(
            default_state_dir(None, home),
            PathBuf::from("/home/u/.local/state/tmux-web")
        );
    }

    #[test]
    fn parse_listen_accepts_socket_addresses_and_names() {
        assert_eq!(
            parse_listen("127.0.0.1:7690").unwrap(),
            addr("127.0.0.1:7690")
        );
        assert_eq!(parse_listen("[::1]:80").unwrap(), addr("[::1]:80"));
        assert!(parse_listen("localhost:7690").unwrap().port() == 7690);
        assert!(parse_listen("nonsense").is_err());
        assert!(parse_listen("127.0.0.1").is_err());
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback(&addr("127.0.0.1:1")));
        assert!(is_loopback(&addr("127.1.2.3:1")));
        assert!(is_loopback(&addr("[::1]:1")));
        assert!(is_loopback(&addr("[::ffff:127.0.0.1]:1")));
        assert!(!is_loopback(&addr("0.0.0.0:1")));
        assert!(!is_loopback(&addr("192.168.1.5:1")));
        assert!(!is_loopback(&addr("[::]:1")));
    }

    #[test]
    fn auth_plan_covers_every_combination() {
        let loopback = addr("127.0.0.1:1");
        let lan = addr("192.168.1.5:1");
        let plan = |a: &SocketAddr, t: Option<&str>, n: bool| plan_auth(a, t, n).unwrap();
        // Loopback: open unless a token is given.
        assert_eq!(plan(&loopback, None, false), AuthPlan::Disabled);
        assert_eq!(plan(&loopback, None, true), AuthPlan::Disabled);
        assert_eq!(
            plan(&loopback, Some("tok"), false),
            AuthPlan::Explicit("tok".into())
        );
        // Non-loopback: never open without --no-auth.
        assert_eq!(plan(&lan, None, false), AuthPlan::StateFile);
        assert_eq!(plan(&lan, None, true), AuthPlan::Disabled);
        assert_eq!(
            plan(&lan, Some("tok"), false),
            AuthPlan::Explicit("tok".into())
        );
        // Contradictory.
        assert!(plan_auth(&lan, Some("tok"), true).is_err());
        assert!(plan_auth(&loopback, Some("tok"), true).is_err());
    }

    #[test]
    fn tokens_are_restricted_to_url_safe_characters() {
        assert!(validate_token("abc-DEF_123.~").is_ok());
        for bad in [
            "", "a b", "a;b", "a,b", "a\"b", "a\\b", "a%41", "a&b", "a#b", "a/b", "é",
        ] {
            assert!(validate_token(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn token_file_is_generated_once_with_mode_0600_and_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let (first, generated) = load_or_create_token(dir.path()).unwrap();
        assert!(generated);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        let path = dir.path().join(TOKEN_FILE);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (second, generated) = load_or_create_token(dir.path()).unwrap();
        assert!(!generated);
        assert_eq!(second, first);
    }

    #[test]
    fn token_file_with_a_trailing_newline_and_lax_mode_is_accepted_and_tightened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        std::fs::write(&path, "hand-made-token\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (token, generated) = load_or_create_token(dir.path()).unwrap();
        assert_eq!(token, "hand-made-token");
        assert!(!generated);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn an_invalid_token_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(TOKEN_FILE);
        std::fs::write(&path, "has spaces in it").unwrap();
        let (token, generated) = load_or_create_token(dir.path()).unwrap();
        assert!(generated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), token);
    }

    #[test]
    fn for_serve_applies_options_and_validates_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let state_arg = state.to_str().unwrap();
        let cli = parse(&[
            "--state-dir",
            state_arg,
            "--claude-config-dir",
            "/nonexistent/claude",
            "-L",
            "sock",
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--token",
            "tok-123",
            "--allow-host",
            "Box.Example.",
            "--poll-interval-ms",
            "50",
            "--max-upload-mb",
            "2",
            "--upload-retention-days",
            "1",
        ])
        .unwrap();
        let config = Config::from_cli(&cli).unwrap();
        assert_eq!(config.state_dir, state);
        assert_eq!(
            config.claude_config_dir,
            PathBuf::from("/nonexistent/claude")
        );
        assert_eq!(config.tmux.socket, SocketSel::Name("sock".into()));
        assert_eq!(config.listen.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(config.auth.token(), Some("tok-123"));
        assert_eq!(config.allow_hosts, ["box.example"]);
        assert_eq!(config.poll_interval, Duration::from_millis(50));
        assert_eq!(config.max_upload_bytes, 2 * 1024 * 1024);
        assert_eq!(config.upload_retention, Duration::from_secs(86_400));
        assert_eq!(config.uploads_dir(), state.join("uploads"));

        for bad in [
            ["--poll-interval-ms", "1"],
            ["--max-upload-mb", "0"],
            ["--upload-retention-days", "0"],
        ] {
            let cli = parse(&["--state-dir", state_arg, "serve", bad[0], bad[1]]).unwrap();
            assert!(Config::from_cli(&cli).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn the_default_session_is_main_unless_one_is_given() {
        assert_eq!(serve_config(&[]).unwrap().default_session, "main");
        let config = serve_config(&["--default-session", "work"]).unwrap();
        assert_eq!(config.default_session, "work");
        // Every valid tmux session name is accepted, spaces and non-ASCII included.
        let config = serve_config(&["--default-session", "mein tmux ünï"]).unwrap();
        assert_eq!(config.default_session, "mein tmux ünï");
    }

    #[test]
    fn the_default_session_can_come_from_the_environment() {
        let command = Cli::command();
        let option = command
            .get_arguments()
            .find(|arg| arg.get_id() == "default_session")
            .expect("--default-session");
        assert_eq!(
            option.get_env().map(|name| name.to_string_lossy()),
            Some("TMUX_WEB_DEFAULT_SESSION".into())
        );
    }

    #[test]
    fn an_invalid_default_session_is_a_startup_error_naming_the_option() {
        let too_long = "x".repeat(101);
        for bad in [
            "a:b",
            "a.b",
            "",
            "-lead",
            " lead",
            "trail ",
            "ctl\u{7}",
            too_long.as_str(),
        ] {
            // The `=` form, so that a leading "-" is a value and not another option.
            let err = serve_config(&[&format!("--default-session={bad}")]).unwrap_err();
            let message = err.to_string();
            assert!(
                message.starts_with(&format!(
                    "--default-session {bad:?} is not a valid session name: "
                )),
                "{bad:?}: {message}"
            );
        }
    }

    #[test]
    fn for_serve_on_a_lan_address_generates_a_token_in_the_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let cli = parse(&[
            "--state-dir",
            state.to_str().unwrap(),
            "serve",
            "--listen",
            "192.168.1.5:7690",
        ])
        .unwrap();
        let config = Config::from_cli(&cli).unwrap();
        let AuthSetup::Token { token, source } = &config.auth else {
            panic!("a token is required on a non-loopback address");
        };
        assert_eq!(*source, TokenSource::Generated);
        assert_eq!(
            std::fs::read_to_string(state.join(TOKEN_FILE)).unwrap(),
            *token
        );
        // A second start reads the same token.
        let again = Config::from_cli(&cli).unwrap();
        assert_eq!(again.auth.token(), Some(token.as_str()));
        assert!(matches!(
            again.auth,
            AuthSetup::Token {
                source: TokenSource::File,
                ..
            }
        ));
    }

    #[test]
    fn log_format_auto_follows_the_terminal() {
        assert!(LogFormat::Auto.is_json(false));
        assert!(!LogFormat::Auto.is_json(true));
        assert!(LogFormat::Json.is_json(true));
        assert!(!LogFormat::Text.is_json(false));
    }
}
