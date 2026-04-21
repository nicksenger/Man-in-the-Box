use super::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder, p3};

pub(crate) async fn run(options: HostOptions) -> Result<(), HostError> {
    install_tls_crypto_provider();
    info!(
        policy_component = %options.policy_component.display(),
        poll_interval_ms = options.poll_interval.as_millis(),
        max_transcript_bytes = options.max_transcript_bytes,
        disable_spawn = options.disable_spawn,
        disable_networking = options.disable_networking,
        disable_filesystem = options.disable_filesystem,
        "starting host runtime"
    );
    let event_sender = options.event_sender.clone();
    let shutdown = options
        .shutdown
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let report_store = ReportStore::new();
    let transcript = Arc::new(Mutex::new(TranscriptBuffer::default()));
    let mut agent_runtime = options.agent.map(|agent_options| {
        agent::spawn(
            agent_options,
            report_store.clone(),
            Arc::clone(&shutdown),
            event_sender.clone(),
        )
    });
    let (mut store, policy_world) = instantiate_policy(
        &options.policy_component,
        options.alias.as_deref(),
        options.disable_spawn,
        options.disable_networking,
        options.disable_filesystem,
        report_store.clone(),
        Arc::clone(&transcript),
    )
    .await?;
    let policy = policy_world.mitb_host_policy_api();
    let session = store
        .run_concurrent(async |accessor| {
            let (session, task_exit) = policy.session().call_constructor(accessor).await?;
            task_exit.block(accessor).await;
            Ok::<wasmtime::component::ResourceAny, wasmtime::Error>(session)
        })
        .await??;
    let pty = PtySession::spawn(
        options.command,
        options.command_args,
        Arc::clone(&transcript),
        options.max_transcript_bytes,
        options.event_sender.clone(),
        options.keyboard_rx.expect("keyboard_rx must be set"),
    )?;

    let mut ticker = tokio::time::interval(options.poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        trace!("host polling tick");

        if let Some(agent_runtime) = &mut agent_runtime
            && let Ok(agent::AgentRuntimeEvent::Fatal(message)) = agent_runtime.events.try_recv()
        {
            warn!(%message, "agent reporting failed");
            shutdown.store(true, Ordering::Relaxed);
            return Err(HostError::Agent(message));
        }

        if shutdown.load(Ordering::Relaxed) {
            info!("shutdown requested by application");
            if let Some(tx) = &event_sender {
                let _ = tx.send(HostEvent::SessionEnded(String::from(
                    "shutdown requested by application",
                )));
            }
            break;
        }

        if pty.child_exited().await? {
            info!("PTY child process exited");
            if let Some(tx) = &event_sender {
                let _ = tx.send(HostEvent::SessionEnded(String::from(
                    "child process exited",
                )));
            }
            break;
        }

        debug!("invoking policy.poll");
        let action = store
            .run_concurrent(async |accessor| {
                let (action, task_exit) = policy.session().call_poll(accessor, session).await?;
                task_exit.block(accessor).await;
                Ok::<Result<bindings::mitb::host::types::Action, String>, wasmtime::Error>(action)
            })
            .await??;

        match action {
            Ok(bindings::mitb::host::types::Action::Perturb(inputs)) => {
                debug!(inputs = inputs.len(), "policy requested perturb");
                pty.write_inputs(inputs).await?;
            }
            Ok(bindings::mitb::host::types::Action::Wait) => {
                debug!("policy requested wait");
            }
            Err(error) => {
                warn!(%error, "policy returned error");
                if let Some(tx) = &event_sender {
                    let _ = tx.send(HostEvent::SessionEnded(format!("policy error: {error}")));
                }
                return Err(HostError::Policy(error));
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);
    let terminate_result = pty.terminate().await;
    if let Some(agent_runtime) = agent_runtime {
        let _ = agent_runtime.handle.await;
    }
    if let Some(tx) = &event_sender {
        let _ = tx.send(HostEvent::Disconnected);
    }
    terminate_result
}

async fn instantiate_policy(
    component_path: &Path,
    alias: Option<&str>,
    disable_spawn: bool,
    disable_networking: bool,
    disable_filesystem: bool,
    report_store: ReportStore,
    transcript: SharedTranscript,
) -> Result<(Store<StoreState>, bindings::Policy), HostError> {
    info!(component = %component_path.display(), "instantiating policy component");
    let engine = create_engine()?;
    let component = Component::from_file(&engine, component_path)?;
    let linker = create_linker(&engine)?;

    let mut wasi = WasiCtxBuilder::new();
    wasi.inherit_stdin()
        .stdout(PolicyLogWriter::new("stdout"))
        .stderr(PolicyLogWriter::new("stderr"))
        .inherit_env();
    if disable_networking {
        wasi.allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .socket_addr_check(|_, _| Box::pin(async { false }));
    }
    if !disable_filesystem && let Some(home_dir) = host_home_dir() {
        wasi.env(MITB_HOME_DIR_ENV, home_dir.to_string_lossy().as_ref());
    }
    if let Some(alias) = alias {
        wasi.env(MITB_ALIAS_ENV, alias);
    }
    if !disable_filesystem {
        let cwd = std::env::current_dir()?;
        info!(cwd = %cwd.display(), "preopening guest cwd for WASI filesystem access");
        wasi.preopened_dir(&cwd, ".", DirPerms::all(), FilePerms::all())?;
        if let Some(shared_root) = host_shared_root_dir() {
            std::fs::create_dir_all(&shared_root)?;
            let shared_root_env_value = shared_root.to_string_lossy().into_owned();
            info!(
                host_path = %shared_root.display(),
                guest_path = MITB_SHARED_ROOT_GUEST_PATH,
                "preopening shared guest directory for cross-worktree coordination"
            );
            wasi.preopened_dir(
                &shared_root,
                MITB_SHARED_ROOT_GUEST_PATH,
                DirPerms::all(),
                FilePerms::all(),
            )?;
            wasi.env(MITB_SHARED_ROOT_ENV, shared_root_env_value.as_str());
        } else {
            warn!(
                "host home directory unavailable; shared coordination directory was not preopened"
            );
        }
    }
    let wasi = wasi.build();
    let state = StoreState {
        wasi,
        http: HostHttpCtx::new(disable_networking),
        table: ResourceTable::new(),
        report_store,
        transcript,
        disable_spawn,
    };

    let mut store = Store::new(&engine, state);
    let policy_world = bindings::Policy::instantiate_async(&mut store, &component, &linker).await?;
    Ok((store, policy_world))
}

fn create_engine() -> Result<Engine, HostError> {
    let mut config = Config::new();
    config.async_support(true);
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    Engine::new(&config).map_err(HostError::from)
}

fn create_linker(engine: &Engine) -> Result<Linker<StoreState>, HostError> {
    let mut linker = Linker::new(engine);
    p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    debug!("registered WASI p3 linker imports");
    bindings::mitb::host::types::add_to_linker::<_, StoreState>(&mut linker, |state| state)?;
    bindings::mitb::host::terminal::add_to_linker::<_, StoreState>(&mut linker, |state| state)?;
    bindings::mitb::host::process::add_to_linker::<_, StoreState>(&mut linker, |state| state)?;
    bindings::mitb::host::reporting::add_to_linker::<_, StoreState>(&mut linker, |state| state)?;
    debug!("registered policy host imports");
    Ok(linker)
}
