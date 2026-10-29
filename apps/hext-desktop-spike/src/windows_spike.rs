#![allow(unsafe_code)] // a spike made of Win32 calls

use std::ffi::c_void;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HMODULE, POINT, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW, HDESK,
    OpenInputDesktop, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
    MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::{Interface, PCWSTR, PWSTR, w};

const SERVICE: &str = "HextSpike";
const POLICY_KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";

fn dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join("HextSpike")
}

/// One line in `log.txt` (and on the console, when there is one).
fn log(role: &str, message: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
    let line = format!("{h:02}:{m:02}:{s:02} UTC [{role}] {message}");
    println!("{line}");
    let _ = fs::create_dir_all(dir());
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir().join("log.txt"))
    {
        let _ = writeln!(file, "{line}");
    }
}

pub fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "install" => install(),
        "uninstall" => uninstall(),
        "sas" => {
            let _ = fs::create_dir_all(dir());
            match fs::write(dir().join("sas"), b"") {
                Ok(()) => println!("pedido de Ctrl+Alt+Del enviado ao serviço (veja o log)"),
                Err(e) => println!("falhou ({e}): rode como administrador"),
            }
        }
        "service" => run_service(),
        "agent" => agent(),
        _ => println!("uso: hext-desktop-spike install | sas | uninstall"),
    }
}

// ---------------------------------------------------------------- install

fn run(program: &str, args: &[&str]) -> bool {
    match Command::new(program).args(args).status() {
        Ok(status) => status.success(),
        Err(e) => {
            println!("{program}: {e}");
            false
        }
    }
}

fn install() {
    let exe = std::env::current_exe().expect("own path");
    let bin_path = format!("\"{}\" service", exe.display());
    // Only SYSTEM and administrators may write here: the service acts on
    // the `sas` file and writes the log as SYSTEM.
    let _ = fs::create_dir_all(dir());
    let folder = dir().display().to_string();
    let private = run(
        "icacls",
        &[
            &folder,
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ],
    );
    let _ = fs::remove_file(dir().join("log.txt"));
    // SendSAS from a service needs this policy (1 = services).
    let policy = run(
        "reg",
        &[
            "add",
            POLICY_KEY,
            "/v",
            "SoftwareSASGeneration",
            "/t",
            "REG_DWORD",
            "/d",
            "1",
            "/f",
        ],
    );
    let created = run(
        "sc.exe",
        &["create", SERVICE, "binPath=", &bin_path, "start=", "demand"],
    );
    let started = run("sc.exe", &["start", SERVICE]);
    if private && policy && created && started {
        println!();
        println!("Instalado. Agora, nesta ordem, anotando o horário de cada passo:");
        println!("  (nas esperas, não mexa no mouse: o teste move o cursor e confere)");
        println!("  1. espere 10 s;");
        println!("  2. Win+L (bloquear), espere 15 s e desbloqueie;");
        println!("  3. abra algo como administrador (prompt do UAC), espere 15 s e cancele;");
        println!("  4. rode `hext-desktop-spike sas` como administrador, espere 15 s e cancele;");
        println!("  5. se puder: saia da conta (logoff), espere 15 s e entre de novo.");
        println!(
            "Depois mande {} e as imagens .bmp.",
            dir().join("log.txt").display()
        );
        println!("Para remover: hext-desktop-spike uninstall");
    } else {
        println!("Algo falhou: rode como administrador.");
    }
}

fn uninstall() {
    run("sc.exe", &["stop", SERVICE]);
    std::thread::sleep(Duration::from_secs(2));
    run("sc.exe", &["delete", SERVICE]);
    run(
        "reg",
        &["delete", POLICY_KEY, "/v", "SoftwareSASGeneration", "/f"],
    );
    println!("Removido. O log e as imagens ficam em {}", dir().display());
}

// ---------------------------------------------------------------- service

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS: OnceLock<usize> = OnceLock::new();

fn run_service() {
    let mut name: Vec<u16> = SERVICE.encode_utf16().chain([0]).collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    if let Err(e) = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        println!("isto é o serviço; use `install` ({e})");
    }
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    let Some(&handle) = STATUS.get() else { return };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP
        } else {
            0
        },
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
    if control == SERVICE_CONTROL_STOP {
        STOP.store(true, Ordering::SeqCst);
    }
    0
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let handle =
        match unsafe { RegisterServiceCtrlHandlerExW(w!("HextSpike"), Some(handler), None) } {
            Ok(handle) => handle,
            Err(e) => {
                log("serviço", &format!("RegisterServiceCtrlHandlerExW: {e}"));
                return;
            }
        };
    let _ = STATUS.set(handle.0 as usize);
    set_state(SERVICE_RUNNING);
    log("serviço", "iniciado");
    supervise();
    set_state(SERVICE_STOPPED);
}

windows_core::link!("sas.dll" "system" fn SendSAS(asuser: windows_core::BOOL));

/// Keeps an agent running in the console session; answers `sas` requests.
fn supervise() {
    let mut agent: Option<(HANDLE, u32)> = None;
    while !STOP.load(Ordering::SeqCst) {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let alive = agent.is_some_and(|(process, s)| {
            s == session && unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0
        });
        if !alive && session != u32::MAX {
            if let Some((process, old)) = agent.take() {
                log(
                    "serviço",
                    &format!("agent da sessão {old} terminou ou trocou"),
                );
                unsafe {
                    let _ = TerminateProcess(process, 0);
                    let _ = CloseHandle(process);
                }
            }
            match launch_agent(session) {
                Ok(process) => {
                    log("serviço", &format!("agent lançado na sessão {session}"));
                    agent = Some((process, session));
                }
                Err(e) => log(
                    "serviço",
                    &format!("lançar o agent na sessão {session}: {e}"),
                ),
            }
        }
        if fs::remove_file(dir().join("sas")).is_ok() {
            unsafe { SendSAS(false.into()) };
            log("serviço", "SendSAS chamado");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if let Some((process, _)) = agent {
        unsafe {
            let _ = TerminateProcess(process, 0);
            let _ = CloseHandle(process);
        }
    }
    log("serviço", "parado");
}

/// Starts `<this exe> agent` as SYSTEM in `session`, on `winsta0\default`.
fn launch_agent(session: u32) -> windows::core::Result<HANDLE> {
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
        let result = (|| {
            SetTokenInformation(
                token,
                TokenSessionId,
                (&raw const session).cast(),
                size_of::<u32>() as u32,
            )?;
            let exe = std::env::current_exe().map_err(|_| windows::core::Error::empty())?;
            let mut command: Vec<u16> = format!("\"{}\" agent", exe.display())
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
            Ok(info.hProcess)
        })();
        let _ = CloseHandle(token);
        result
    }
}

// ---------------------------------------------------------------- agent

/// Follows the input desktop, captures it and pokes the mouse.
fn agent() {
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    log("agent", &format!("iniciado (sessão do console {session})"));
    let mut current = String::new();
    let mut thread_desk: Option<HDESK> = None;
    let mut capture: Option<Duplication> = None;
    let mut frames = 0u32;
    // None: save the next frame (a still screen sends only one).
    let mut last_save: Option<Instant> = None;
    // Each visit to a desktop also keeps its first image, numbered.
    let mut visit = 0u32;
    let mut first_of_visit = true;
    let mut last_poke = Instant::now();
    let mut last_report = Instant::now();
    loop {
        // Which desktop gets input now (Default, Winlogon, Screen-saver...).
        match input_desktop() {
            Ok((desk, name)) if name != current => {
                capture = None; // DXGI objects belong to the old desktop
                match unsafe { SetThreadDesktop(desk) } {
                    Ok(()) => {
                        log("agent", &format!("desktop de entrada: {name}"));
                        if let Some(old) = thread_desk.replace(desk) {
                            let _ = unsafe { CloseDesktop(old) };
                        }
                        current = name;
                        frames = 0;
                        last_save = None;
                        visit += 1;
                        first_of_visit = true;
                    }
                    Err(e) => {
                        // Stays on the old one, and tries again.
                        log("agent", &format!("SetThreadDesktop({name}): {e}"));
                        let _ = unsafe { CloseDesktop(desk) };
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            Ok((desk, _)) => {
                let _ = unsafe { CloseDesktop(desk) };
            }
            Err(e) => {
                log("agent", &format!("OpenInputDesktop: {e}"));
                std::thread::sleep(Duration::from_secs(1));
            }
        }

        if capture.is_none() {
            match Duplication::new() {
                Ok(d) => {
                    log("agent", &format!("captura aberta em {current}"));
                    capture = Some(d);
                }
                Err(e) => {
                    log("agent", &format!("abrir a captura em {current}: {e}"));
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
        if let Some(d) = &mut capture {
            let save = last_save.is_none_or(|t| t.elapsed() >= Duration::from_secs(3));
            match d.next(save) {
                Ok(Some(image)) => {
                    frames += 1;
                    match image {
                        Some(Ok((width, height, pixels))) => {
                            last_save = Some(Instant::now());
                            if std::mem::take(&mut first_of_visit) && visit <= 40 {
                                let path = dir().join(format!("{visit:02}-{current}.bmp"));
                                let _ = write_bmp(&path, width, height, &pixels);
                            }
                            let path = dir().join(format!("{current}.bmp"));
                            if let Err(e) = write_bmp(&path, width, height, &pixels) {
                                log("agent", &format!("salvar {}: {e}", path.display()));
                            }
                        }
                        Some(Err(e)) => log("agent", &format!("copiar o quadro: {e}")),
                        None => {}
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    log("agent", &format!("captura em {current}: {e}"));
                    capture = None;
                }
            }
        }

        // Input is its own question: tried even where capture fails.
        if last_poke.elapsed() >= Duration::from_secs(5) {
            last_poke = Instant::now();
            let result = poke_mouse();
            log("agent", &format!("SendInput em {current}: {result}"));
        }
        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            log("agent", &format!("{current}: {frames} quadros novos"));
            frames = 0;
        }
    }
}

fn input_desktop() -> windows::core::Result<(HDESK, String)> {
    unsafe {
        let desk = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x1000_0000), // GENERIC_ALL
        )?;
        let mut name = [0u16; 128];
        let mut needed = 0;
        let got = GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        );
        if let Err(e) = got {
            let _ = CloseDesktop(desk);
            return Err(e);
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Ok((desk, String::from_utf16_lossy(&name[..len])))
    }
}

/// Moves the mouse one pixel right and back; whether the cursor really
/// moved (a blocked `SendInput` still says it sent).
fn poke_mouse() -> String {
    let position = || {
        let mut p = POINT::default();
        unsafe { GetCursorPos(&mut p) }.map(|()| (p.x, p.y)).ok()
    };
    let Some(before) = position() else {
        return format!("GetCursorPos falhou ({:?})", unsafe { GetLastError() });
    };
    // An absolute move to a known point (a relative one-pixel move can be
    // eaten by pointer acceleration), then back.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let target = (
        if before.0 - 40 >= vx {
            before.0 - 40
        } else {
            before.0 + 40
        },
        before.1,
    );
    let to = |(x, y): (i32, i32)| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                // 0..=65535 across the whole virtual desktop.
                dx: ((x - vx) as i64 * 65535 / (vw - 1) as i64) as i32,
                dy: ((y - vy) as i64 * 65535 / (vh - 1) as i64) as i32,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ..Default::default()
            },
        },
    };
    let sent = unsafe { SendInput(&[to(target)], size_of::<INPUT>() as i32) };
    std::thread::sleep(Duration::from_millis(30));
    let after = position();
    let _ = unsafe { SendInput(&[to(before)], size_of::<INPUT>() as i32) };
    if sent != 1 {
        return format!("recusado ({:?})", unsafe { GetLastError() });
    }
    match after {
        Some(a) if (a.0 - target.0).abs() <= 1 && (a.1 - target.1).abs() <= 1 => {
            format!("ok, o cursor foi para {a:?} e voltou")
        }
        Some(a) => format!("NÃO: pedi {target:?}, o cursor está em {a:?}"),
        None => format!("enviado; GetCursorPos falhou ({:?})", unsafe {
            GetLastError()
        }),
    }
}

/// DXGI Desktop Duplication of the primary display, on the thread's desktop.
struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
}

type Image = (u32, u32, Vec<u8>);

impl Duplication {
    fn new() -> windows::core::Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.ok_or_else(windows::core::Error::empty)?;
            let context = context.ok_or_else(windows::core::Error::empty)?;
            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let output: IDXGIOutput1 = adapter.EnumOutputs(0)?.cast()?;
            let duplication = output.DuplicateOutput(&device)?;
            Ok(Self {
                device,
                context,
                duplication,
            })
        }
    }

    /// `Some` when the image changed (not just the pointer); with the
    /// pixels (or why not) when `copy`.
    #[allow(clippy::type_complexity)]
    fn next(
        &mut self,
        copy: bool,
    ) -> windows::core::Result<Option<Option<windows::core::Result<Image>>>> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe {
            self.duplication
                .AcquireNextFrame(200, &mut info, &mut resource)
        } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            // ACCESS_LOST (desktop switch) and the rest: open it again.
            Err(e) => return Err(e),
        }
        // Only the pointer moved (our own poke, for one): not a new image.
        let changed = info.LastPresentTime != 0;
        let image = match (&resource, copy && changed) {
            (Some(r), true) => Some(self.copy(r)),
            _ => None,
        };
        let _ = unsafe { self.duplication.ReleaseFrame() };
        Ok(changed.then_some(image))
    }

    fn copy(&self, resource: &IDXGIResource) -> windows::core::Result<Image> {
        unsafe {
            let texture: ID3D11Texture2D = resource.cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                BindFlags: 0,
                MiscFlags: 0,
                MipLevels: 1,
                ArraySize: 1,
                ..desc
            };
            let mut staging = None;
            self.device
                .CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
            let staging = staging.ok_or_else(windows::core::Error::empty)?;
            self.context.CopyResource(&staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row = desc.Width as usize * 4;
            let mut pixels = Vec::with_capacity(row * desc.Height as usize);
            for y in 0..desc.Height as usize {
                let start = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                pixels.extend_from_slice(std::slice::from_raw_parts(start, row));
            }
            self.context.Unmap(&staging, 0);
            Ok((desc.Width, desc.Height, pixels))
        }
    }
}

/// A 32-bit top-down BMP from BGRA rows.
fn write_bmp(path: &Path, width: u32, height: u32, pixels: &[u8]) -> std::io::Result<()> {
    let mut bmp = Vec::with_capacity(54 + pixels.len());
    let size = 54 + pixels.len() as u32;
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&size.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(width as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(height as i32)).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    bmp.extend_from_slice(pixels);
    fs::write(path, bmp)
}
