//! Control-plane operations on a [`Conn`]: listing, loading capture sinks, moving streams,
//! and undoing it all (when a source stops, from the watchdog, and in the startup sweep).

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use pulseaudio::protocol::{self, Command, Prop, PulseError};

use super::conn::{CallError, Conn, get_str};
use super::modargs::{self, ServerKind};
use super::routing::{self, Moved, ROLE_KEY, Sink, Stream};

pub struct ServerState {
    pub kind: ServerKind,
    pub default_sink: Option<String>,
}

pub fn server(conn: &mut Conn) -> anyhow::Result<ServerState> {
    let info: protocol::ServerInfo = conn.request(&Command::GetServerInfo).context("reading server info")?;
    let text = |s: &Option<CString>| s.as_ref().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(ServerState {
        kind: ServerKind::detect(&text(&info.server_name), &text(&info.server_version)),
        default_sink: info.default_sink_name.map(|c| c.to_string_lossy().into_owned()).filter(|s| !s.is_empty()),
    })
}

pub fn sinks(conn: &mut Conn) -> anyhow::Result<Vec<Sink>> {
    let list: protocol::SinkInfoList = conn.request(&Command::GetSinkInfoList).context("listing outputs")?;
    Ok(list.iter().map(to_sink).collect())
}

pub fn streams(conn: &mut Conn) -> anyhow::Result<Vec<Stream>> {
    let list: protocol::SinkInputInfoList =
        conn.request(&Command::GetSinkInputInfoList).context("listing app streams")?;
    Ok(list.iter().map(to_stream).collect())
}

/// The sink called `name`, if it exists.
pub fn find_sink(conn: &mut Conn, name: &str) -> anyhow::Result<Option<Sink>> {
    let query = protocol::GetSinkInfo { index: None, name: Some(cstring(name)?) };
    match conn.request::<protocol::SinkInfo>(&Command::GetSinkInfo(query)) {
        Ok(info) => Ok(Some(to_sink(&info))),
        Err(e) if e.is(PulseError::NoEntity) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("looking up output {name}")),
    }
}

pub fn move_stream(conn: &mut Conn, index: u32, sink_name: &str) -> Result<(), CallError> {
    let device_name = CString::new(sink_name).map_err(|_| CallError::Server(PulseError::Invalid))?;
    // Exactly one of device index and name may be set (both servers check this).
    conn.ack(&Command::MoveSinkInput(protocol::MoveStreamParams {
        index: Some(index),
        device_index: None,
        device_name: Some(device_name),
    }))
}

pub fn set_default_sink(conn: &mut Conn, name: &str) -> Result<(), CallError> {
    let name = CString::new(name).map_err(|_| CallError::Server(PulseError::Invalid))?;
    conn.ack(&Command::SetDefaultSink(name))
}

/// The server does not let clients load modules, so no source can be captured however often
/// it is retried. Found in an error chain with [`is_module_loading_denied`].
#[derive(Debug)]
pub struct ModuleLoadingDenied;

impl fmt::Display for ModuleLoadingDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "the sound server does not let apps load modules (on PipeWire this is \
             `pulse.allow-module-loading = false`), so Sidestage cannot capture an app",
        )
    }
}

impl std::error::Error for ModuleLoadingDenied {}

pub fn is_module_loading_denied(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<ModuleLoadingDenied>())
}

/// Loads a capture sink and returns its module index. Tries the per-server argument variants in
/// order (see [`modargs::null_sink_args`]); `label` names the source's app in its description.
pub fn load_capture_sink(conn: &mut Conn, kind: &ServerKind, sink_name: &str, label: &str) -> anyhow::Result<u32> {
    let mut last = None;
    for args in modargs::null_sink_args(kind, sink_name, label) {
        let params =
            protocol::LoadModuleParams { name: c"module-null-sink".to_owned(), arguments: Some(cstring(&args)?) };
        match conn.request::<protocol::LookupReply>(&Command::LoadModule(params)) {
            Ok(reply) => {
                log::info!("pulse: loaded module-null-sink #{} ({args})", reply.0);
                return Ok(reply.0);
            }
            Err(CallError::Server(code)) => {
                log::warn!("pulse: module-null-sink {args} was refused: {code}");
                if code == PulseError::AccessDenied {
                    return Err(ModuleLoadingDenied.into());
                }
                last = Some(code);
            }
            Err(e) => return Err(e).context("loading the Sidestage capture output"),
        }
    }
    Err(anyhow!(
        "the sound server refused to create Sidestage's capture output ({})",
        last.map_or_else(|| "unknown error".to_owned(), |c| c.to_string())
    ))
}

/// Waits for a just-loaded sink to appear. pipewire-pulse may answer the load before the
/// node is visible to other requests.
pub fn wait_for_sink(conn: &mut Conn, name: &str, timeout: Duration) -> anyhow::Result<Sink> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(sink) = find_sink(conn, name)? {
            return Ok(sink);
        }
        if Instant::now() > deadline {
            bail!("the Sidestage capture output did not appear");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Unloads a module, treating "already gone" as success.
pub fn unload(conn: &mut Conn, module: u32) -> anyhow::Result<()> {
    match conn.ack(&Command::UnloadModule(module)) {
        Ok(()) => Ok(()),
        Err(e) if e.is(PulseError::NoEntity) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("unloading module #{module}")),
    }
}

/// Moves every stream off the capture sink `sink_name` (to its original sink or the current
/// default, see [`routing::plan_restore`]) and unloads the sink's module. The module is found
/// on the live server (the sink's owner, or the null-sink module whose arguments name it), never
/// from a journaled index, which a restarted server may have reused. The module is looked for
/// even when the sink is not listed: a crash right after loading it, or a load whose sink never
/// showed up, leaves a module whose sink may appear later. Returns plain-language lines.
pub fn restore_sink(conn: &mut Conn, sink_name: &str, tracked: &[Moved]) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::new();
    let sinks = sinks(conn)?;
    let capture = sinks.iter().find(|s| s.name == sink_name);
    let failed = match capture {
        Some(capture) => move_all_off(conn, capture.index, &sinks, tracked, &mut lines)?,
        None => 0,
    };
    // The owner index comes from the live server, so it cannot be a stale index from before a
    // server restart; the argument match covers servers that do not report owners (or, like
    // pipewire-pulse, may report a PipeWire module as the owner).
    let owner = capture.and_then(|c| c.owner_module);
    let mut unloaded = 0;
    for m in null_sink_modules(conn)?.iter().filter(|m| is_module_of(m, sink_name, owner)) {
        unload(conn, m.index)?;
        unloaded += 1;
        log::info!("pulse: unloaded module #{} ({sink_name})", m.index);
    }
    if capture.is_none() && unloaded == 0 {
        log::info!("pulse: capture sink {sink_name} is already gone");
    }
    if failed > 0 {
        lines.push(format!(
            "{failed} stream(s) could not be moved back directly; the sound server moved them to the default output."
        ));
    }
    Ok(lines)
}

/// Moves every stream on the capture sink back; returns how many the server refused to move.
fn move_all_off(
    conn: &mut Conn,
    capture: u32,
    sinks: &[Sink],
    tracked: &[Moved],
    lines: &mut Vec<String>,
) -> anyhow::Result<usize> {
    let default = server(conn)?.default_sink;
    let streams = streams(conn)?;
    let mut failed = 0;
    for (index, to) in routing::plan_restore(&streams, capture, sinks, tracked, default.as_deref()) {
        let app = streams.iter().find(|s| s.index == index).map_or("An app", Stream::label).to_owned();
        let to_label = sinks.iter().find(|s| s.name == to).map_or(to.as_str(), Sink::label).to_owned();
        match move_stream(conn, index, &to) {
            Ok(()) => {
                log::info!("pulse: moved sink-input #{index} ({app}) back to {to}");
                lines.push(format!("{app} was moved back to {to_label}."));
            }
            Err(CallError::Server(PulseError::NoEntity)) => {
                log::debug!("pulse: sink-input #{index} ({app}) ended meanwhile");
            }
            Err(CallError::Server(code)) => {
                // Unloading the module below makes the server rescue it anyway.
                log::warn!("pulse: could not move sink-input #{index} ({app}) back to {to}: {code}");
                failed += 1;
            }
            Err(e) => return Err(e).context("moving apps back to their outputs"),
        }
    }
    Ok(failed)
}

/// Loaded `module-null-sink` instances.
fn null_sink_modules(conn: &mut Conn) -> anyhow::Result<protocol::ModuleInfoList> {
    let mut modules: protocol::ModuleInfoList = conn.request(&Command::GetModuleInfoList).context("listing modules")?;
    modules.retain(|m| m.name.to_bytes() == b"module-null-sink");
    Ok(modules)
}

/// The capture sink a null-sink module was loaded for, if it is a Sidestage one.
fn module_sink(m: &protocol::ModuleInfo) -> Option<String> {
    let args = m.argument.as_ref()?.to_string_lossy();
    modargs::sidestage_sink_in_args(&args).map(str::to_owned)
}

/// Whether null-sink module `m` created the sink `sink_name`, whose owner the server reports as
/// `owner` (see [`modargs::created_sink`]).
fn is_module_of(m: &protocol::ModuleInfo, sink_name: &str, owner: Option<u32>) -> bool {
    let args = m.argument.as_ref().map(|a| a.to_string_lossy()).unwrap_or_default();
    modargs::created_sink(&args, m.index, sink_name, owner)
}

/// Removes every Sidestage capture sink except those in `keep` (a live session's), moving the
/// apps on them back. `journal` supplies original sinks for the sinks it describes. Modules
/// whose sink is not listed (yet) are removed as well.
pub fn sweep(
    conn: &mut Conn,
    journal: Option<&routing::RoutingJournal>,
    keep: &BTreeSet<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut leftovers: BTreeSet<String> = sinks(conn)?.into_iter().filter(|s| s.is_capture()).map(|s| s.name).collect();
    leftovers.extend(null_sink_modules(conn)?.iter().filter_map(module_sink));
    leftovers.retain(|name| !keep.contains(name.as_str()));
    let mut lines = Vec::new();
    for name in leftovers {
        let tracked = journal
            .and_then(|j| j.sinks.iter().find(|s| s.sink_name == name))
            .map(|s| s.streams.as_slice())
            .unwrap_or_default();
        log::info!("pulse: removing leftover capture sink {name}");
        lines.extend(restore_sink(conn, &name, tracked)?);
        lines.push("Removed a virtual output that an earlier Sidestage run left behind.".to_owned());
    }
    Ok(lines)
}

fn to_sink(info: &protocol::SinkInfo) -> Sink {
    Sink {
        index: info.index,
        name: info.name.to_string_lossy().into_owned(),
        description: info.description.as_ref().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default(),
        monitor_source: info.monitor_source_index,
        owner_module: info.owner_module_index,
    }
}

fn to_stream(info: &protocol::SinkInputInfo) -> Stream {
    let p = &info.props;
    let binary = get_str(p, Prop::ApplicationProcessBinary.to_c_str());
    let name = get_str(p, Prop::ApplicationName.to_c_str());
    let pid = get_str(p, Prop::ApplicationProcessId.to_c_str()).and_then(|s| s.parse().ok());
    let stream_name = info.name.to_string_lossy().trim().to_owned();
    Stream {
        index: info.index,
        sink: info.sink_index,
        app_key: routing::app_key(binary.as_deref(), name.as_deref(), pid),
        app_name: name.or(binary),
        media_name: get_str(p, Prop::MediaName.to_c_str()).or((!stream_name.is_empty()).then_some(stream_name)),
        pid,
        own_role: get_str(p, ROLE_KEY),
        corked: info.corked,
    }
}

fn cstring(s: &str) -> anyhow::Result<CString> {
    CString::new(s).with_context(|| format!("{s:?} contains a NUL byte"))
}
