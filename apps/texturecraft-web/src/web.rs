//! The eframe web runner: canvas lookup, backend selection, the loading screen's startup steps,
//! browser shortcuts the app takes over, the unsaved-changes guard on close, and startup errors
//! shown in the page.

use eframe::egui;
use texturecraft_engine::Session;
use texturecraft_ui_egui::TextureCraftApp;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;

use crate::account::{self, AccountView};
use crate::account_port::AccountPort;

const CANVAS_ID: &str = "texturecraft_canvas";
const LOADING_ID: &str = "texturecraft_loading";
const STATUS_ID: &str = "texturecraft_status";
const PROGRESS_ID: &str = "texturecraft_progress";

pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    capture_app_shortcuts();
    let hosted = query().trim_start_matches('?').split('&').any(|item| item == "host=webos-account");
    if !hosted {
        guard_unsaved_changes();
    }
    wasm_bindgen_futures::spawn_local(async move {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            log::error!("no document");
            return;
        };
        let result = match document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) {
            Some(canvas) => run(canvas, hosted).await,
            None => Err(format!("the page has no <canvas id=\"{CANVAS_ID}\">")),
        };
        if let Some(el) = document.get_element_by_id(LOADING_ID) {
            match result {
                Ok(()) => el.remove(),
                Err(e) => {
                    log::error!("TextureCraft failed to start: {e}");
                    el.set_text_content(Some(&format!(
                        "TextureCraft failed to start: {e}. It needs a browser with WebGPU (current Chrome, Edge or Safari) or at least WebGL2."
                    )));
                }
            }
        }
    });
}

async fn run(canvas: web_sys::HtmlCanvasElement, hosted: bool) -> Result<(), String> {
    // The WebAssembly has downloaded and compiled when this runs.
    let (session, account) = if hosted {
        step(0.5, "Opening the account project…").await;
        let port = AccountPort::connect()?;
        let (session, identity) = account::open(&port).await?;
        (session, Some((port, identity)))
    } else {
        step(0.5, "Preparing the app…").await;
        (Session::closed().map_err(|e| e.to_string())?, None)
    };
    step(0.8, "Starting the renderer…").await;
    let mut options = eframe::WebOptions::default();
    if query().contains("webgl")
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
    {
        create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
    }
    let port = account.as_ref().map(|(port, _)| port.clone());
    let started = eframe::WebRunner::new()
        .start(
            canvas,
            options,
            Box::new(|cc| {
                let (mut app, account) = if let Some(port) = port {
                    port.wake_on(cc.egui_ctx.clone());
                    let view = AccountView::new(port);
                    let app = TextureCraftApp::new_for_account(cc, session, view.project_service(cc.egui_ctx.clone()));
                    (app, Some(view))
                } else {
                    (TextureCraftApp::new(cc, session), None)
                };
                if let Some(rs) = &cc.wgpu_render_state {
                    let backend = rs.adapter.get_info().backend;
                    log::info!("TextureCraft: wgpu backend {backend:?}");
                    if backend == eframe::wgpu::Backend::Gl {
                        app.banner = Some(texturecraft_ui_egui::WEBGL_NOTICE);
                    }
                }
                Ok(Box::new(Editor { app, account }))
            }),
        )
        .await
        .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")));
    if let Some((port, identity)) = account {
        match &started {
            Ok(()) => port.publish("started", identity, None)?,
            Err(error) => port.startup_failed(error),
        }
    }
    started
}

struct Editor {
    app: TextureCraftApp,
    account: Option<AccountView>,
}

impl eframe::App for Editor {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Some(account) = &mut self.account {
            account.poll(&mut self.app);
            if account.suspended() {
                return;
            }
        }
        self.app.logic(ctx, frame);
        if let Some(account) = &mut self.account
            && let Err(error) = account.publish_state(&self.app)
        {
            self.app.toast(error, true);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        if self.account.as_ref().is_some_and(AccountView::suspended) {
            return;
        }
        self.app.ui(ui, frame);
    }

    fn persist_egui_memory(&self) -> bool {
        self.app.persist_egui_memory()
    }

    fn auto_save_interval(&self) -> std::time::Duration {
        self.app.auto_save_interval()
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.app.save(storage);
    }
}

/// Show a startup step on the loading screen and let the browser paint it before the next
/// (blocking) step runs.
async fn step(progress: f32, status: &str) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
    if let Some(el) = document.get_element_by_id(STATUS_ID) {
        el.set_text_content(Some(status));
    }
    if let Some(el) = document.get_element_by_id(PROGRESS_ID) {
        let _ = el.set_attribute("value", &progress.to_string());
    }
    // A timer longer than a frame lets the browser paint the step before work resumes. Not
    // `requestAnimationFrame`: it never fires in a background tab, which would stall startup.
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 20);
        }
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}

/// The app's shortcuts (Ctrl+S, Ctrl+O, Ctrl+D, F5…) would also trigger the browser's own
/// actions; take them first. Copy, cut and paste stay with the browser, which turns them into
/// the clipboard events egui reads. Shortcuts the browser reserves (Ctrl+N, Ctrl+T, Ctrl+W)
/// never reach the page.
fn capture_app_shortcuts() {
    let Some(window) = web_sys::window() else { return };
    let on_key = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(|e: web_sys::KeyboardEvent| {
        let key = eframe::egui::Key::from_name(&e.key()).or_else(|| eframe::egui::Key::from_name(&e.code()));
        let Some(key) = key else { return };
        let mods = eframe::egui::Modifiers {
            alt: e.alt_key(),
            ctrl: e.ctrl_key(),
            shift: e.shift_key(),
            mac_cmd: e.meta_key(),
            command: e.ctrl_key() || e.meta_key(),
        };
        if texturecraft_ui_egui::keymap::captured(mods, key) {
            e.prevent_default();
        }
    });
    let options = web_sys::AddEventListenerOptions::new();
    options.set_capture(true);
    if let Err(e) = window.add_event_listener_with_callback_and_add_event_listener_options("keydown", on_key.as_ref().unchecked_ref(), &options) {
        log::warn!("couldn't take over the browser's shortcuts: {e:?}");
    }
    // The listener lives as long as the page.
    on_key.forget();
}

/// Closing or reloading the tab with unsaved changes makes the browser ask first (it shows its
/// own message; pages can't word it).
fn guard_unsaved_changes() {
    let Some(window) = web_sys::window() else { return };
    let on_unload = Closure::<dyn FnMut(web_sys::BeforeUnloadEvent)>::new(|e: web_sys::BeforeUnloadEvent| {
        if texturecraft_ui_egui::web_files::unsaved() {
            e.prevent_default();
            // Browsers that predate `preventDefault` here read the return value.
            e.set_return_value("unsaved");
        }
    });
    if let Err(e) = window.add_event_listener_with_callback("beforeunload", on_unload.as_ref().unchecked_ref()) {
        log::warn!("couldn't guard unsaved changes on close: {e:?}");
    }
    on_unload.forget();
}

fn query() -> String {
    web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
}
