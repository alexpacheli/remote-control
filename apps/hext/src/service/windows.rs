//! The Windows service (`HextRemote`): runs as SYSTEM from boot, keeps the
//! computer online with its own ID and owns the sessions; an agent in the
//! console session (relaunched when that session changes) captures and
//! injects input there. As tested in the secure-desktop spike.
#![allow(unsafe_code)] // Win32 calls: services, tokens, processes, pipes

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hext_codec::EncoderSettings;
use hext_crypto::{FileKeyStore, load_or_create, machine_dir};
use hext_net::EndpointConfig;
use hext_proto::PeerInfo;
use hext_session::SessionCodes;
use hext_session::host::{BoxFuture, HostConfig};
use hext_signal::DeviceId;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::{Notify, Semaphore};
use tracing::{info, warn};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_SERVICE_SPECIFIC_ERROR, HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    DuplicateTokenEx, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
    TokenPrimary, TokenSessionId, WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTSGetActiveConsoleSessionId, WTSSendMessageW,
};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING,
    SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    StartServiceCtrlDispatcherW,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::IsUserAnAdmin;
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MESSAGEBOX_RESULT,
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::agent::{AgentHub, Stream};
use crate::signaling::Ask;
use crate::{APP_VERSION, DEFAULT_RELAY, DEFAULT_SERVER, device_name};

const SERVICE: &str = "HextRemote";
/// How long the user has to answer a request (the server gives up at 60 s).
const ANSWER_SECS: u32 = 55;

/// Where the computer's ID is written for `service status`.
fn id_file() -> Result<PathBuf> {
    Ok(machine_dir()?.join("id.txt"))
}

/// Where the installed program lives.
fn program_dir() -> Result<PathBuf> {
    let base = std::env::var_os("ProgramFiles").context("sem %ProgramFiles%")?;
    Ok(PathBuf::from(base).join("Hext Remote"))
}

// ---------------------------------------------------------------- commands

fn sc(args: &[&str]) -> Result<String> {
    let out = Command::new("sc.exe")
        .args(args)
        .output()
        .context("sc.exe")?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(text)
    } else {
        bail!("sc {}: {}", args.join(" "), text.trim())
    }
}

fn exists() -> bool {
    sc(&["query", SERVICE]).is_ok()
}

/// Stops the service and waits until it has.
fn stop() {
    if sc(&["stop", SERVICE]).is_err() {
        return; // not running
    }
    for _ in 0..40 {
        match sc(&["query", SERVICE]) {
            Ok(state) if !state.contains("STOPPED") => {
                std::thread::sleep(Duration::from_millis(500));
            }
            _ => return,
        }
    }
}

fn require_admin() -> Result<()> {
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        bail!("rode como administrador (terminal aberto com \"Executar como administrador\")");
    }
    Ok(())
}

fn icacls(args: &[&str]) -> Result<()> {
    let out = Command::new("icacls")
        .args(args)
        .output()
        .context("icacls")?;
    if !out.status.success() {
        bail!(
            "icacls {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

/// Whether `path` belongs to the system or the administrators (not to an
/// account that could have put a key of its own there).
fn owned_by_admins(path: &Path) -> Result<bool> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut descriptor,
        )
    };
    if status.is_err() {
        bail!("dono de {}: {status:?}", path.display());
    }
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    Ok(trusted)
}

/// The computer's folder (the key, the ID and the logs), open only to the
/// system and the administrators: the key is sealed for the machine, so
/// any account that could read the file could use it.
fn prepare_machine_dir() -> Result<PathBuf> {
    let data = machine_dir()?;
    if data.exists() && !owned_by_admins(&data)? {
        // Someone else made it (before any install): nothing in it is ours.
        let aside = data.with_file_name(format!(
            "HextRemote.desconhecida-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        ));
        std::fs::rename(&data, &aside).with_context(|| data.display().to_string())?;
        println!(
            "{} não foi criada pelo Hext Remote: movida para {}",
            data.display(),
            aside.display()
        );
    }
    std::fs::create_dir_all(&data).with_context(|| data.display().to_string())?;
    let folder = data.display().to_string();
    icacls(&[&folder, "/setowner", "*S-1-5-32-544", "/T", "/C", "/Q"])?;
    icacls(&[&folder, "/reset", "/T", "/C", "/Q"])?;
    icacls(&[
        &folder,
        "/inheritance:r",
        "/grant:r",
        "*S-1-5-18:(OI)(CI)F",
        "*S-1-5-32-544:(OI)(CI)F",
        "/T",
        "/C",
        "/Q",
    ])?;
    Ok(data)
}

pub fn install() -> Result<()> {
    require_admin()?;
    let data = prepare_machine_dir()?;

    // The program, where the service runs it from (an update replaces it).
    let installed = exists();
    if installed {
        println!("Atualizando o serviço...");
        stop();
    }
    let dir = program_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    let exe = dir.join("HextRemote.exe");
    let current = std::env::current_exe()?;
    if !same_file(&current, &exe) {
        std::fs::copy(&current, &exe).with_context(|| format!("copiar para {}", exe.display()))?;
    }

    let bin_path = format!("\"{}\" service run", exe.display());
    if installed {
        sc(&["config", SERVICE, "binPath=", &bin_path, "start=", "auto"])?;
    } else {
        sc(&[
            "create",
            SERVICE,
            "binPath=",
            &bin_path,
            "start=",
            "auto",
            "DisplayName=",
            "Hext Remote",
        ])?;
    }
    sc(&[
        "description",
        SERVICE,
        "Mantém este computador online com o seu ID do Hext Remote, para receber pedidos de acesso.",
    ])?;
    // Restarted if it ever fails, also when it stops with an error.
    sc(&[
        "failure",
        SERVICE,
        "reset=",
        "86400",
        "actions=",
        "restart/5000/restart/5000/restart/60000",
    ])?;
    sc(&["failureflag", SERVICE, "1"])?;
    let _ = std::fs::remove_file(id_file()?);
    sc(&["start", SERVICE])?;

    println!("Serviço instalado e iniciado.");
    for _ in 0..30 {
        if let Ok(id) = std::fs::read_to_string(id_file()?) {
            println!("ID deste computador: {}", id.trim());
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!(
        "Ainda sem ID: o servidor não respondeu. Veja {}",
        data.join("service.log").display()
    );
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub fn uninstall(purge: bool) -> Result<()> {
    require_admin()?;
    if exists() {
        stop();
        sc(&["delete", SERVICE])?;
        println!("Serviço removido.");
    } else {
        println!("O serviço não está instalado.");
    }
    let dir = program_dir()?;
    if dir.exists() && std::fs::remove_dir_all(&dir).is_err() {
        println!("Não deu para apagar {} (em uso?)", dir.display());
    }
    if purge {
        let data = machine_dir()?;
        if data.exists() {
            std::fs::remove_dir_all(&data).with_context(|| data.display().to_string())?;
            println!("Identidade deste computador apagada: o ID dele deixou de valer.");
        }
    }
    Ok(())
}

pub fn status() -> Result<()> {
    match sc(&["query", SERVICE]) {
        Ok(state) if state.contains("RUNNING") => println!("Serviço: rodando"),
        Ok(state) if state.contains("STOPPED") => println!("Serviço: parado"),
        Ok(_) => println!("Serviço: iniciando ou parando"),
        Err(_) => {
            println!("Serviço: não instalado (HextRemote service install)");
            return Ok(());
        }
    }
    match std::fs::read_to_string(id_file()?) {
        Ok(id) => println!("ID deste computador: {}", id.trim()),
        Err(_) => println!("ID: ainda não (ou rode como administrador para ler)"),
    }
    Ok(())
}

// ---------------------------------------------------------------- service

/// Wakes the service's work when Windows asks it to stop.
static STOP: Notify = Notify::const_new();
static STATUS: OnceLock<usize> = OnceLock::new();

/// Called by Windows (through `service run`).
pub fn run() -> Result<()> {
    let mut name: Vec<u16> = SERVICE.encode_utf16().chain([0]).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
        .context("isto é o serviço, iniciado pelo Windows; use `service install`")?;
    Ok(())
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    report(state, false);
}

/// Tells Windows the service's state; `failed`: it stopped on an error
/// (which starts the restart actions).
fn report(state: SERVICE_STATUS_CURRENT_STATE, failed: bool) {
    let Some(&handle) = STATUS.get() else { return };
    let status = SERVICE_STATUS {
        dwWin32ExitCode: if failed {
            ERROR_SERVICE_SPECIFIC_ERROR.0
        } else {
            0
        },
        dwServiceSpecificExitCode: u32::from(failed),
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWaitHint: 10_000,
        ..Default::default()
    };
    let _ = unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(handle as *mut c_void), &status) };
}

unsafe extern "system" fn handler(
    control: u32,
    _event: u32,
    _data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        set_state(SERVICE_STOP_PENDING);
        STOP.notify_one();
    }
    0
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let handle =
        match unsafe { RegisterServiceCtrlHandlerExW(w!("HextRemote"), Some(handler), None) } {
            Ok(handle) => handle,
            Err(e) => {
                warn!("RegisterServiceCtrlHandlerExW: {e}");
                return;
            }
        };
    let _ = STATUS.set(handle.0 as usize);
    set_state(SERVICE_START_PENDING);
    let result = tokio::runtime::Runtime::new()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| {
            let result = runtime.block_on(serve());
            // Not waiting for a question still on screen (it times out).
            runtime.shutdown_timeout(Duration::from_secs(2));
            result
        });
    match &result {
        Ok(()) => info!("serviço parado"),
        Err(e) => warn!("serviço parado por erro: {e:#}"),
    }
    report(SERVICE_STOPPED, result.is_err());
}

/// The host, as a service: by ID only (no access code), the user asked
/// through a message box, the screen through the agents.
async fn serve() -> Result<()> {
    info!("serviço iniciado (v{APP_VERSION})");
    let key =
        Arc::new(load_or_create(&FileKeyStore::machine(machine_dir()?.join("device.key")))?.0);
    let name = device_name();
    // Registered before the relay sees this computer (it serves only
    // registered ones); the ID goes to `service status`.
    match tokio::time::timeout(
        Duration::from_secs(10),
        crate::signaling::login(DEFAULT_SERVER, &key, &name),
    )
    .await
    {
        Ok(Ok(client)) => {
            write_id(client.id());
            let _ = client.close().await;
        }
        Ok(Err(e)) => warn!("{e:#}"),
        Err(_) => warn!("servidor de sinalização: sem resposta"),
    }
    let relay = crate::relay(DEFAULT_RELAY)?;
    let endpoint = hext_net::bind(
        &key,
        &EndpointConfig {
            relay,
            bind: None,
            accept: true,
        },
    )
    .await?;
    set_state(SERVICE_RUNNING);

    let hub = AgentHub::default();
    tokio::spawn(supervise(hub.clone()));
    let encoder = EncoderSettings::default();
    let session_codes = SessionCodes::default();
    let config = Arc::new(HostConfig {
        peer: PeerInfo::local(APP_VERSION, name.clone()),
        // Only viewers the user accepted, with their one-time codes.
        access_code: None,
        session_codes: session_codes.clone(),
        desktop: Arc::new(hub.desktop(Stream {
            display: 0,
            test_pattern: false,
            encoder,
        })),
    });
    let slot = Arc::new(Semaphore::new(1));
    tokio::spawn(crate::signaling::host(
        DEFAULT_SERVER.to_owned(),
        key,
        name,
        (endpoint.clone(), true),
        session_codes,
        slot.clone(),
        Box::new(MessageBox),
    ));
    crate::host::accept(&endpoint, config, slot, STOP.notified()).await;
    // A session still on: the agent stops streaming and releases any held
    // key before the service goes.
    hub.stop();
    tokio::time::sleep(Duration::from_millis(500)).await;
    endpoint.close().await;
    Ok(())
}

fn write_id(id: DeviceId) {
    if let Ok(path) = id_file()
        && let Err(e) = std::fs::write(&path, id.to_string())
    {
        warn!("{}: {e}", path.display());
    }
    info!("ID deste computador: {id}");
}

/// Asks whoever is at the console, in a message box.
struct MessageBox;

impl Ask for MessageBox {
    fn ask<'a>(&'a mut self, name: &'a str, id: DeviceId) -> BoxFuture<'a, bool> {
        let title: Vec<u16> = "Hext Remote".encode_utf16().collect();
        let text: Vec<u16> =
            format!("\"{name}\" (ID {id}) quer acessar este computador.\n\nAceitar?")
                .encode_utf16()
                .collect();
        Box::pin(async move {
            let asked = tokio::task::spawn_blocking(move || {
                let session = unsafe { WTSGetActiveConsoleSessionId() };
                let mut answer = MESSAGEBOX_RESULT(0);
                // Waits for the answer, or `ANSWER_SECS` (then it is a no).
                let sent = unsafe {
                    WTSSendMessageW(
                        Some(WTS_CURRENT_SERVER_HANDLE),
                        session,
                        PCWSTR(title.as_ptr()),
                        (title.len() * 2) as u32,
                        PCWSTR(text.as_ptr()),
                        (text.len() * 2) as u32,
                        MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
                        ANSWER_SECS,
                        &mut answer,
                        true,
                    )
                };
                match sent {
                    Ok(()) => answer == IDYES,
                    Err(e) => {
                        warn!("perguntar na sessão {session}: {e}");
                        false
                    }
                }
            });
            asked.await.unwrap_or(false)
        })
    }

    fn show_id(&mut self, id: DeviceId) {
        write_id(id);
    }
}

// ---------------------------------------------------------------- agents

/// A process handle, closed with its owner.
struct Process(HANDLE);

// The handle is only a number to the kernel.
unsafe impl Send for Process {}

impl Process {
    fn alive(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0, 0) }) != WAIT_OBJECT_0
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = TerminateProcess(self.0, 0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Keeps an agent running in the console session: a new one when the
/// session changes (a logon, switching users) or the agent ends.
async fn supervise(hub: AgentHub) {
    let mut current: Option<(u32, Process, tokio::task::JoinHandle<()>)> = None;
    // After a failure, waits longer each time (and logs less).
    let mut retry = Duration::from_secs(1);
    loop {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let alive = current
            .as_ref()
            .is_some_and(|(s, process, _)| *s == session && process.alive());
        // u32::MAX: no session attached to the console (switching).
        if !alive && session != u32::MAX {
            if let Some((old, process, attached)) = current.take() {
                info!("agent da sessão {old} encerrado");
                drop(process);
                attached.abort();
            }
            match launch(session, &hub).await {
                Ok((process, attached)) => {
                    info!("agent iniciado na sessão {session}");
                    current = Some((session, process, attached));
                    retry = Duration::from_secs(1);
                }
                Err(e) => {
                    warn!("iniciar o agent na sessão {session}: {e:#} (de novo em {retry:?})");
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(Duration::from_secs(60));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Starts an agent in `session`, connected to `hub` through a new pipe
/// that only the system can open, and checks it is that agent on the line.
async fn launch(session: u32, hub: &AgentHub) -> Result<(Process, tokio::task::JoinHandle<()>)> {
    let mut nonce = [0u8; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("sem números aleatórios"))?;
    let name = format!(r"\\.\pipe\hext-agent-{}", hex(&nonce));
    let pipe = system_only_pipe(&name)?;
    let (process, pid) = spawn_agent(session, &name)?;
    tokio::time::timeout(Duration::from_secs(15), pipe.connect())
        .await
        .context("o agent não se conectou")??;
    let mut client = 0;
    unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut client) }?;
    if client != pid {
        bail!("processo {client} no lugar do agent {pid}");
    }
    let hub = hub.clone();
    let attached = tokio::spawn(async move { hub.attach(pipe).await });
    Ok((process, attached))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A named pipe only SYSTEM can open (the agent runs as SYSTEM).
fn system_only_pipe(name: &str) -> Result<NamedPipeServer> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;;GA;;;SY)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let pipe = unsafe {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    Ok(pipe?)
}

/// Starts `<this exe> agent --pipe <name>` as SYSTEM in `session`.
fn spawn_agent(session: u32, pipe: &str) -> Result<(Process, u32)> {
    let exe = std::env::current_exe()?;
    unsafe {
        let mut own = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own)?;
        let mut token = HANDLE::default();
        let duplicated = DuplicateTokenEx(
            own,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        );
        let _ = CloseHandle(own);
        duplicated?;
        let result = (|| -> Result<(Process, u32)> {
            SetTokenInformation(
                token,
                TokenSessionId,
                (&raw const session).cast(),
                size_of::<u32>() as u32,
            )?;
            let mut command: Vec<u16> = format!("\"{}\" agent --pipe {pipe}", exe.display())
                .encode_utf16()
                .chain([0])
                .collect();
            let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
            let startup = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                ..Default::default()
            };
            let mut info = PROCESS_INFORMATION::default();
            CreateProcessAsUserW(
                Some(token),
                PCWSTR::null(),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &startup,
                &mut info,
            )?;
            let _ = CloseHandle(info.hThread);
            Ok((Process(info.hProcess), info.dwProcessId))
        })();
        let _ = CloseHandle(token);
        result
    }
}

// ---------------------------------------------------------------- agent

/// The agent: connects to the service's pipe and streams this session's
/// screen while the service asks.
pub fn agent(pipe: &str) -> Result<()> {
    info!("agent iniciado (sessão {})", unsafe {
        WTSGetActiveConsoleSessionId()
    });
    tokio::runtime::Runtime::new()?.block_on(async {
        let client = ClientOptions::new()
            .open(pipe)
            .with_context(|| format!("abrir {pipe}"))?;
        crate::agent::serve(client).await?;
        info!("o serviço encerrou o agent");
        Ok(())
    })
}
