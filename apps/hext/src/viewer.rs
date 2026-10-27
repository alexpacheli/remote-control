//! The viewer: a window showing the host's screen, forwarding input.
//!
//! winit owns the main thread (required on macOS); networking runs on a
//! tokio runtime and decoding on its own thread. Each decoded frame is
//! handed over through `latest` and the event loop is woken to draw it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hext_codec::YuvFrame;
use hext_input::keymap::{self, CommandKey};
use hext_proto::Os;
use hext_proto::{Hello, InputEvent, MouseButton, PeerInfo};
use hext_render::Renderer;
use hext_session::client::{self, ClientHandle};
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::{APP_VERSION, CmdKey, CtrlKey, ViewArgs, device_name, parse_addr};

/// Trackpad scrolling arrives in pixels; this many make one wheel notch.
const PIXELS_PER_NOTCH: f64 = 50.0;

enum UserEvent {
    Frame,
    SessionEnded,
}

pub fn run(args: ViewArgs) -> Result<()> {
    let addr = parse_addr(&args.addr)?;
    if args.fingerprint.is_none() {
        warn!("sem --fingerprint: a conexão é cifrada, mas a identidade do host NÃO é verificada");
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let hello = Hello {
        peer: PeerInfo::local(APP_VERSION, device_name()),
        access_code: args.code.clone(),
    };

    if args.headless {
        return runtime.block_on(headless(addr, args, hello));
    }

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let latest: Arc<Mutex<Option<YuvFrame>>> = Arc::default();

    let (handle, closed_proxy) = (latest.clone(), proxy.clone());
    let session = runtime.block_on(async {
        let endpoint = hext_net::client_endpoint(addr)?;
        let conn = hext_net::connect(&endpoint, addr, args.fingerprint)
            .await
            .with_context(|| format!("falha ao conectar em {addr}"))?;
        let watch = conn.clone();
        let session = client::start(conn, hello, move |frame| {
            *handle.lock().expect("lock") = Some(frame.yuv);
            let _ = proxy.send_event(UserEvent::Frame);
        })
        .await?;
        tokio::spawn(async move {
            watch.closed().await;
            let _ = closed_proxy.send_event(UserEvent::SessionEnded);
        });
        // Keep the endpoint alive as long as the runtime.
        tokio::spawn(async move { endpoint.wait_idle().await });
        anyhow::Ok(session)
    })?;
    info!(
        "conectado a {} ({:?}, v{})",
        session.host.device_name, session.host.os, session.host.app_version
    );

    let host_is_mac = session.host.os == Os::MacOs;
    let mut app = Viewer {
        runtime,
        session: Some(session),
        latest,
        window: None,
        renderer: None,
        command_key: match args.cmd_key {
            // A Mac host understands Command itself.
            _ if host_is_mac => CommandKey::Win,
            CmdKey::Ctrl => CommandKey::Ctrl,
            CmdKey::Win => CommandKey::Win,
        },
        swap_ctrl_and_super: host_is_mac
            && !cfg!(target_os = "macos")
            && args.ctrl_key == CtrlKey::Cmd,
        last_title: Instant::now(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct Viewer {
    runtime: tokio::runtime::Runtime,
    session: Option<ClientHandle>,
    latest: Arc<Mutex<Option<YuvFrame>>>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    command_key: CommandKey,
    /// Windows/Linux keyboard on a Mac host: Ctrl acts as Command.
    swap_ctrl_and_super: bool,
    last_title: Instant,
}

impl Viewer {
    fn send(&self, event: InputEvent) {
        if let Some(session) = &self.session {
            session.send_input(event);
        }
    }

    /// Uploads the newest decoded frame, if any, and redraws.
    ///
    /// With `only_if_new`, does nothing when no new frame arrived: several
    /// frame events can find a single frame, and presenting the same picture
    /// again could block on vsync and back up the event queue.
    fn draw(&mut self, only_if_new: bool) {
        let frame = self.latest.lock().expect("lock").take();
        let Some(renderer) = &mut self.renderer else {
            return;
        };
        match frame {
            Some(frame) => renderer.upload(&frame),
            None if only_if_new => return,
            None => {}
        }
        if let Err(e) = renderer.render() {
            warn!("render: {e}");
        }
    }

    fn update_title(&mut self) {
        if self.last_title.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_title = Instant::now();
        let (Some(window), Some(session)) = (&self.window, &self.session) else {
            return;
        };
        let mut stats = session.stats.lock().expect("lock");
        let mut title = format!("Hext Remote — {}", session.host.device_name);
        if let Some(snapshot) = stats.snapshot() {
            title += &format!(
                " — {}x{} | {snapshot} | RTT {:.1} ms",
                stats.width,
                stats.height,
                session.rtt().as_secs_f64() * 1000.0
            );
        }
        window.set_title(&title);
    }
}

impl ApplicationHandler<UserEvent> for Viewer {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let host = self
            .session
            .as_ref()
            .map_or_else(String::new, |s| s.host.device_name.clone());
        let attributes = Window::default_attributes()
            .with_title(format!("Hext Remote — {host}"))
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                warn!("janela: {e}");
                event_loop.exit();
                return;
            }
        };
        match Renderer::new(window.clone()) {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(e) => {
                warn!("{e}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            // Draw right away instead of waiting for a redraw request: lower latency.
            UserEvent::Frame => self.draw(true),
            UserEvent::SessionEnded => {
                info!("sessão encerrada pelo host");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                self.draw(false);
            }
            WindowEvent::RedrawRequested => self.draw(false),
            WindowEvent::Focused(false) => self.send(InputEvent::ReleaseAll),
            WindowEvent::CursorMoved { position, .. } => {
                let rect = self.renderer.as_ref().and_then(Renderer::video_rect);
                if let Some((x, y)) = rect.and_then(|r| r.normalize(position.x, position.y)) {
                    self.send(InputEvent::MouseMove { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Back,
                    winit::event::MouseButton::Forward => MouseButton::Forward,
                    winit::event::MouseButton::Other(_) => return,
                };
                self.send(InputEvent::MouseButton {
                    button,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (
                        (p.x / PIXELS_PER_NOTCH) as f32,
                        (p.y / PIXELS_PER_NOTCH) as f32,
                    ),
                };
                // winit: positive x moves the content right, i.e. scrolls left.
                // The protocol (like Windows) uses positive dx = scroll right.
                let (dx, dy) = (-x, y);
                if dx != 0.0 || dy != 0.0 {
                    self.send(InputEvent::Wheel { dx, dy });
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                // Repeats are forwarded too: injected keys do not auto-repeat.
                if let Some(mut scancode) = keymap::scancode(code, self.command_key) {
                    if self.swap_ctrl_and_super {
                        scancode = keymap::swap_ctrl_and_super(scancode);
                    }
                    self.send(InputEvent::Key {
                        scancode,
                        pressed: event.state == ElementState::Pressed,
                    });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.update_title();
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(session) = self.session.take()
            && let Err(e) = self.runtime.block_on(session.close())
        {
            warn!("encerrando: {e}");
        }
    }
}

/// Receives and decodes without a window, printing stats every second.
async fn headless(addr: std::net::SocketAddr, args: ViewArgs, hello: Hello) -> Result<()> {
    let endpoint = hext_net::client_endpoint(addr)?;
    let conn = hext_net::connect(&endpoint, addr, args.fingerprint)
        .await
        .with_context(|| format!("falha ao conectar em {addr}"))?;
    let session = client::start(conn, hello, |_| {}).await?;
    println!(
        "Conectado a {} ({:?}, v{})",
        session.host.device_name, session.host.os, session.host.app_version
    );
    for _ in 0..args.seconds {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if session.is_finished() {
            println!("Sessão encerrada pelo host");
            break;
        }
        let mut stats = session.stats.lock().expect("lock");
        let (w, h) = (stats.width, stats.height);
        if let Some(snapshot) = stats.snapshot() {
            println!(
                "{w}x{h} | {snapshot} | RTT {:.1} ms",
                session.rtt().as_secs_f64() * 1000.0
            );
        }
    }
    session.close().await?;
    endpoint.wait_idle().await;
    Ok(())
}
