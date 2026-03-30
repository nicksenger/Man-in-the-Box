use clap::{Arg, ArgAction, Command, value_parser};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use thiserror::Error;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Debug)]
struct CliOptions {
    policy_component: PathBuf,
    command: String,
    command_args: Vec<String>,
    disable_spawn: bool,
    disable_networking: bool,
    disable_filesystem: bool,
    alias: Option<String>,
    interval_ms: u64,
    max_transcript_bytes: usize,
    headless: bool,
}

#[derive(Debug, Error)]
enum CliError {
    #[error("missing required CLI argument: {0}")]
    MissingArgument(&'static str),
    #[error("invalid value for `{field}`: {message}")]
    InvalidArgument {
        field: &'static str,
        message: String,
    },
    #[error(transparent)]
    Host(#[from] mitb_host::HostError),
    #[error(transparent)]
    BoxUi(#[from] mitb_box::BoxError),
    #[error("failed to initialize host runtime: {0}")]
    RuntimeInit(std::io::Error),
    #[error("host thread panicked")]
    HostThreadPanic,
}

fn cli() -> Command {
    Command::new("mitb")
        .about("Run a WASM policy against a PTY-managed child process")
        .arg(
            Arg::new("policy")
                .value_name("POLICY_WASM")
                .required(true)
                .help("Path to the policy component (.wasm)"),
        )
        .arg(
            Arg::new("command")
                .value_name("COMMAND")
                .required(false)
                .conflicts_with("cmd")
                .help("Command to launch directly inside the PTY"),
        )
        .arg(
            Arg::new("command_args")
                .value_name("COMMAND_ARGS")
                .num_args(0..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true)
                .help("Arguments forwarded to COMMAND"),
        )
        .arg(
            Arg::new("cmd")
                .long("cmd")
                .value_name("BASH_COMMAND")
                .conflicts_with("command")
                .help("Run BASH_COMMAND through `$SHELL -ic` inside the PTY"),
        )
        .arg(
            Arg::new("alias")
                .long("alias")
                .value_name("ALIAS")
                .help("Optional agent identifier shown in remote reports"),
        )
        .arg(
            Arg::new("disable_spawn")
                .long("disable-spawn")
                .action(ArgAction::SetTrue)
                .help("Disable guest process spawning through host process APIs"),
        )
        .arg(
            Arg::new("disable_networking")
                .long("disable-networking")
                .action(ArgAction::SetTrue)
                .help("Disable guest networking capabilities, including WASI HTTP"),
        )
        .arg(
            Arg::new("disable_filesystem")
                .long("disable-filesystem")
                .action(ArgAction::SetTrue)
                .help("Disable guest filesystem access by removing host preopens"),
        )
        .arg(
            Arg::new("interval_ms")
                .long("interval-ms")
                .value_name("MILLISECONDS")
                .default_value("2000")
                .value_parser(value_parser!(u64))
                .help("Polling interval for policy evaluation"),
        )
        .arg(
            Arg::new("max_transcript_bytes")
                .long("max-transcript-bytes")
                .value_name("BYTES")
                .default_value("524288")
                .value_parser(value_parser!(usize))
                .help("Maximum PTY transcript retained for policy input"),
        )
        .arg(
            Arg::new("headless")
                .long("headless")
                .action(ArgAction::SetTrue)
                .help("Disable the Man in the Box PTY window"),
        )
}

fn parse_options() -> Result<CliOptions, CliError> {
    let matches = cli().get_matches();

    let policy_component = matches
        .get_one::<String>("policy")
        .cloned()
        .ok_or(CliError::MissingArgument("policy"))
        .map(PathBuf::from)?;

    let command = matches.get_one::<String>("command").cloned();
    let shell_command = matches.get_one::<String>("cmd").cloned();
    let command_args = matches
        .get_many::<String>("command_args")
        .map(|values| values.cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let alias = matches
        .get_one::<String>("alias")
        .map(|value| value.trim().to_owned());
    let disable_spawn = matches.get_flag("disable_spawn");
    let disable_networking = matches.get_flag("disable_networking");
    let disable_filesystem = matches.get_flag("disable_filesystem");

    let interval_ms = matches
        .get_one::<u64>("interval_ms")
        .copied()
        .ok_or(CliError::MissingArgument("interval_ms"))?;

    let max_transcript_bytes = matches
        .get_one::<usize>("max_transcript_bytes")
        .copied()
        .ok_or(CliError::MissingArgument("max_transcript_bytes"))?;
    let headless = matches.get_flag("headless");

    if interval_ms == 0 {
        return Err(CliError::InvalidArgument {
            field: "interval_ms",
            message: "must be greater than zero".to_string(),
        });
    }

    if max_transcript_bytes == 0 {
        return Err(CliError::InvalidArgument {
            field: "max_transcript_bytes",
            message: "must be greater than zero".to_string(),
        });
    }

    if matches!(alias.as_deref(), Some("")) {
        return Err(CliError::InvalidArgument {
            field: "alias",
            message: "must not be empty".to_string(),
        });
    }

    let (command, command_args) = match (command, shell_command) {
        (Some(command), None) => (command, command_args),
        (None, Some(shell_command)) => {
            if shell_command.trim().is_empty() {
                return Err(CliError::InvalidArgument {
                    field: "cmd",
                    message: "must not be empty".to_string(),
                });
            }
            if !command_args.is_empty() {
                return Err(CliError::InvalidArgument {
                    field: "command_args",
                    message: "cannot be used with `--cmd`".to_string(),
                });
            }
            let shell = std::env::var("SHELL")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| CliError::InvalidArgument {
                    field: "cmd",
                    message: "requires `$SHELL` to be set to a shell executable".to_string(),
                })?;
            (shell, vec![String::from("-ic"), shell_command])
        }
        (Some(_), Some(_)) => {
            return Err(CliError::InvalidArgument {
                field: "command",
                message: "cannot be used with `--cmd`".to_string(),
            });
        }
        (None, None) => return Err(CliError::MissingArgument("command or --cmd")),
    };

    Ok(CliOptions {
        policy_component,
        command,
        command_args,
        disable_spawn,
        disable_networking,
        disable_filesystem,
        alias,
        interval_ms,
        max_transcript_bytes,
        headless,
    })
}

fn main() -> Result<(), CliError> {
    init_logging();

    let options = parse_options()?;
    info!(
        policy = %options.policy_component.display(),
        command = %options.command,
        disable_spawn = options.disable_spawn,
        disable_networking = options.disable_networking,
        disable_filesystem = options.disable_filesystem,
        alias = options.alias.as_deref().unwrap_or(""),
        interval_ms = options.interval_ms,
        max_transcript_bytes = options.max_transcript_bytes,
        headless = options.headless,
        "starting mitb"
    );

    let mut host_options = mitb_host::HostOptions::new(
        options.policy_component,
        options.command,
        options.command_args,
    );
    host_options.disable_spawn = options.disable_spawn;
    host_options.disable_networking = options.disable_networking;
    host_options.disable_filesystem = options.disable_filesystem;
    host_options.alias = options.alias.clone();
    host_options.poll_interval = Duration::from_millis(options.interval_ms);
    host_options.max_transcript_bytes = options.max_transcript_bytes;
    host_options.agent = agent_options_from_env(options.alias.clone());

    if options.headless {
        debug!("running in headless mode");
        run_host_blocking(host_options)?;
        return Ok(());
    }

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    host_options.event_sender = Some(event_tx);
    host_options.shutdown = Some(Arc::clone(&shutdown));

    debug!("starting host thread and box UI");
    let host_thread = std::thread::spawn(move || run_host_blocking(host_options));

    let box_result = mitb_box::run(event_rx);
    shutdown.store(true, Ordering::Relaxed);

    let host_result = match host_thread.join() {
        Ok(result) => result,
        Err(_) => Err(CliError::HostThreadPanic),
    };

    box_result?;
    host_result?;
    info!("mitb exited");
    Ok(())
}

fn run_host_blocking(host_options: mitb_host::HostOptions) -> Result<(), CliError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(CliError::RuntimeInit)?;
    runtime.block_on(mitb_host::run(host_options))?;
    Ok(())
}

fn init_logging() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::new("mitb=info,mitb_host=info,info"),
    };

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .finish();

    if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("failed to initialize tracing subscriber: {error}");
    }
}

fn agent_options_from_env(alias: Option<String>) -> Option<mitb_host::AgentOptions> {
    let server_addr_env = std::env::var("MITB_SERVER_ADDR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let secret_code_env = std::env::var("MITB_SECRET_CODE")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());

    match (server_addr_env, secret_code_env) {
        (Some(server_addr), Some(secret_code)) => Some(mitb_host::AgentOptions {
            server_addr,
            secret_code,
            alias,
        }),
        (None, None) => None,
        (Some(_), None) => {
            warn!(
                "MITB_SERVER_ADDR is set but MITB_SECRET_CODE is missing or empty; remote reporting is disabled. Set both MITB_SERVER_ADDR and MITB_SECRET_CODE to enable it."
            );
            None
        }
        (None, Some(_)) => {
            warn!(
                "MITB_SECRET_CODE is set but MITB_SERVER_ADDR is missing or empty; remote reporting is disabled. Set both MITB_SERVER_ADDR and MITB_SECRET_CODE to enable it."
            );
            None
        }
    }
}
