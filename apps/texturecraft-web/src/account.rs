//! TextureCraft design operations through the WebOS account host.

use std::{cell::RefCell, rc::Rc};

use serde_json::{Value, json};
use texturecraft_engine::{Session, file_cmds::PreparedAccountSave};
use texturecraft_ui_egui::{AccountProjectRequest, TextureCraftApp, state::NewProject};
use wasm_bindgen::JsValue;

use crate::account_port::{AccountPort, field, string};

pub(crate) struct AccountView {
    port: AccountPort,
    action: Rc<RefCell<Option<AccountProjectRequest>>>,
    last_state: Value,
    pending: Option<Save>,
    dialog: Option<(String, Option<u64>)>,
    sequence: u64,
    lease: Option<String>,
    finalized: bool,
}

struct Save {
    ticket: String,
    document: String,
    revision: String,
    path: String,
    project: PreparedAccountSave,
}

impl AccountView {
    pub(crate) fn new(port: AccountPort) -> Self {
        Self { port, action: Rc::default(), last_state: Value::Null, pending: None, dialog: None, sequence: 0, lease: None, finalized: false }
    }

    pub(crate) fn project_service(&self, ctx: eframe::egui::Context) -> impl Fn(AccountProjectRequest) -> Result<(), String> + 'static {
        let action = self.action.clone();
        move |request| {
            let mut next = action.try_borrow_mut().map_err(|_| "TextureCraft is processing a project request.")?;
            if next.is_some() {
                return Err("TextureCraft is processing a project request.".into());
            }
            *next = Some(request);
            ctx.request_repaint();
            Ok(())
        }
    }

    pub(crate) fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.dialog.is_some()
    }

    fn unfinished(app: &TextureCraftApp) -> bool {
        !app.session.account_project_capturable() || app.bake.is_some() || app.resize.is_some() || !app.link_cooks.is_empty()
    }

    fn state(&self, app: &TextureCraftApp) -> Value {
        let session = &app.session;
        let unfinished = Self::unfinished(app);
        json!({"available":true,"documentId":session.project_id.to_string(),"revision":session.revision.to_string(),
            "recoveryRevision":session.revision.to_string(),"dirty":session.modified,"capturable":!unfinished,
            "unfinishedEdit":unfinished,"autosaveEligible":false,"operationPending":self.busy()})
    }

    fn targets(app: &TextureCraftApp, packet: &JsValue) -> bool {
        let session = &app.session;
        number(packet, "documentId") == Some(session.project_id) && number(packet, "revision") == Some(session.revision)
    }

    pub(crate) fn poll(&mut self, app: &mut TextureCraftApp) {
        for (kind, packet) in self.port.take_messages() {
            if let Err(error) = self.receive(app, &kind, &packet) {
                app.toast(error, true);
            }
        }
        let action = self.action.borrow_mut().take();
        if let Some(action) = action
            && let Err(error) = self.project_action(app, action)
        {
            app.toast(error, true);
        }
    }

    pub(crate) fn publish_state(&mut self, app: &TextureCraftApp) -> Result<(), String> {
        let state = self.state(app);
        if state != self.last_state {
            self.port.publish("project-state", state.clone(), None)?;
            self.last_state = state;
        }
        Ok(())
    }

    fn receive(&mut self, app: &mut TextureCraftApp, kind: &str, packet: &JsValue) -> Result<(), String> {
        if kind == "save-result" {
            return self.settle_save(app, packet);
        }
        let Some(id) = bounded(packet, "requestId", 36) else { return Ok(()) };
        match kind {
            "recovery-capture" => {
                let mut reply = json!({"requestId":id,"documentId":bounded(packet,"documentId",20),"revision":bounded(packet,"revision",20)});
                if Self::targets(app, packet) && !Self::unfinished(app) && !self.suspended() {
                    match app.session.capture_account_draft() {
                        Ok(bytes) => return self.port.publish("recovery-captured", reply, Some(bytes)),
                        Err(error) => reply["message"] = json!(error.to_string()),
                    }
                } else {
                    // Let the host retire this draft request before its correlated reply arrives.
                    self.publish_state(app)?;
                    reply["message"] = json!("The TextureCraft project changed before recovery capture.");
                }
                self.port.publish("recovery-captured", reply, None)
            }
            "session-save" => self.capture_session_save(app, packet, id),
            "session" => self.session(app, packet, id),
            "dialog-result" => self.dialog_result(app, packet, id),
            "operation" => self.operation(app, packet, id),
            _ => Ok(()),
        }
    }

    fn next_id(&mut self) -> Result<String, String> {
        self.sequence = self.sequence.checked_add(1).ok_or("TextureCraft exhausted its request identifiers.")?;
        Ok(self.sequence.to_string())
    }

    fn request_dialog(&mut self, mut packet: Value, revision: Option<u64>) -> Result<(), String> {
        let id = self.next_id()?;
        packet["requestId"] = json!(id);
        self.port.publish("dialog", packet, None)?;
        self.dialog = Some((id, revision));
        Ok(())
    }

    fn project_action(&mut self, app: &mut TextureCraftApp, action: AccountProjectRequest) -> Result<(), String> {
        if self.suspended() || self.busy() {
            return Err("Finish the pending TextureCraft project operation first.".into());
        }
        let session = &app.session;
        match action {
            AccountProjectRequest::Close => self.port.publish("close-intent", json!({}), None),
            AccountProjectRequest::New(settings) => {
                if settings.file.is_some() || !settings.baked_maps.is_empty() {
                    return Err("Choose a built-in mesh for this new TextureCraft project.".into());
                }
                let text = serde_json::to_string(&json!({"project": settings})).map_err(|error| error.to_string())?;
                self.request_dialog(json!({"operation":"new","settings":text}), None)
            }
            AccountProjectRequest::Sample => self.request_dialog(json!({"operation":"new","settings":"{\"sample\":true}"}), None),
            AccountProjectRequest::Open => self.request_dialog(json!({"operation":"open","path":null}), None),
            AccountProjectRequest::Save | AccountProjectRequest::SaveAs => {
                if Self::unfinished(app) {
                    return Err("Finish the current TextureCraft edit before saving.".into());
                }
                if matches!(action, AccountProjectRequest::Save)
                    && let Some(path) = &session.path
                {
                    return self.send_save(app, path.clone(), "save", json!({}));
                }
                let suggested = format!("{}.tcraft", texturecraft_engine::formats::export::sanitize_file_stem(&session.doc.project.name));
                self.request_dialog(json!({"operation":"save","suggested":suggested}), Some(session.revision))
            }
        }
    }

    fn dialog_result(&mut self, app: &TextureCraftApp, packet: &JsValue, id: String) -> Result<(), String> {
        if !self.dialog.as_ref().is_some_and(|(expected, _)| expected == &id) {
            return Ok(());
        }
        let Some((_, revision)) = self.dialog.take() else { return Ok(()) };
        match bounded(packet, "outcome", 16).as_deref() {
            Some("failed") => Err(bounded(packet, "message", 250).unwrap_or_else(|| "The TextureCraft dialog failed.".into())),
            Some("destination") if revision.is_some() => {
                if revision != Some(app.session.revision) || Self::unfinished(app) || self.suspended() {
                    return Err("The project changed while choosing its save destination.".into());
                }
                let path = bounded(packet, "path", 2048).ok_or("Invalid TextureCraft save destination.")?;
                self.send_save(app, path, "save", json!({}))
            }
            _ => Ok(()),
        }
    }

    fn capture_session_save(&mut self, app: &TextureCraftApp, packet: &JsValue, id: String) -> Result<(), String> {
        let path = bounded(packet, "path", 2048).ok_or("Invalid TextureCraft save destination.")?;
        let mut reply = json!({"requestId":id,"path":path,"documentId":bounded(packet,"documentId",20),
            "reviewedRevision":bounded(packet,"revision",20)});
        if self.busy() || self.suspended() || Self::unfinished(app) || !Self::targets(app, packet) {
            reply["outcome"] = json!("busy");
            return self.port.publish("session-save-result", reply, None);
        }
        if let Err(error) = self.send_save(app, path, "session-save-result", reply.clone()) {
            reply["outcome"] = json!("failed");
            reply["message"] = json!(error);
            self.port.publish("session-save-result", reply, None)?;
        }
        Ok(())
    }

    fn send_save(&mut self, app: &TextureCraftApp, path: String, kind: &str, mut reply: Value) -> Result<(), String> {
        let (project, bytes) = app.session.prepare_account_save(&path).map_err(|error| error.to_string())?;
        let ticket = self.next_id()?;
        let document = app.session.project_id.to_string();
        let revision = app.session.revision.to_string();
        reply["outcome"] = json!("captured");
        reply["ticket"] = json!(ticket);
        reply["documentId"] = json!(document);
        reply["revision"] = json!(revision);
        reply["path"] = json!(path);
        self.port.publish(kind, reply, Some(bytes))?;
        self.pending = Some(Save { ticket, document, revision, path, project });
        Ok(())
    }

    fn settle_save(&mut self, app: &mut TextureCraftApp, packet: &JsValue) -> Result<(), String> {
        let matching = self.pending.as_ref().is_some_and(|save| {
            bounded(packet, "ticket", 20).as_deref() == Some(save.ticket.as_str())
                && bounded(packet, "documentId", 20).as_deref() == Some(save.document.as_str())
                && bounded(packet, "revision", 20).as_deref() == Some(save.revision.as_str())
                && bounded(packet, "path", 2048).as_deref() == Some(save.path.as_str())
        });
        let outcome = bounded(packet, "outcome", 16);
        if !matching || !matches!(outcome.as_deref(), Some("written" | "failed" | "unknown")) {
            return Ok(());
        }
        let Some(save) = self.pending.take() else { return Ok(()) };
        let adopted = outcome.as_deref() == Some("written") && app.session.adopt_account_save(save.project).is_ok();
        self.port.publish(
            "save-settled",
            json!({"ticket":save.ticket,"documentId":save.document,"revision":save.revision,
            "path":save.path,"adopted":adopted,"projectState":self.state(app)}),
            None,
        )
    }

    fn session(&mut self, app: &mut TextureCraftApp, packet: &JsValue, id: String) -> Result<(), String> {
        let operation = bounded(packet, "operation", 16).ok_or("Invalid TextureCraft session operation.")?;
        let lease = bounded(packet, "lease", 64).filter(|value| !value.is_empty()).ok_or("Invalid TextureCraft session lease.")?;
        let accepted = match operation.as_str() {
            "pause"
                if !self.suspended()
                    && !self.busy()
                    && !Self::unfinished(app)
                    && Self::targets(app, packet)
                    && field(packet, "dirty").as_bool() == Some(app.session.modified)
                    && field(packet, "unfinishedEdit").as_bool() == Some(false) =>
            {
                self.lease = Some(lease.clone());
                true
            }
            "release" | "finalize" if self.lease.as_deref() == Some(lease.as_str()) => {
                self.lease = None;
                self.finalized = operation == "finalize";
                true
            }
            _ => false,
        };
        self.port.publish(
            "session-result",
            json!({"requestId":id,"operation":operation,"lease":lease,"ok":accepted,
            "message":if accepted {""} else {"The TextureCraft project changed after close review."}}),
            None,
        )
    }

    fn operation(&self, app: &mut TextureCraftApp, packet: &JsValue, id: String) -> Result<(), String> {
        let document = bounded(packet, "documentId", 20).ok_or("Invalid TextureCraft project target.")?;
        let mut reply = json!({"requestId":id,"documentId":document});
        let path = field(packet, "path");
        let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid TextureCraft project binding.")?) };
        let session = &app.session;
        let result = if self.suspended() || document != session.project_id.to_string() || path != session.path {
            Err("The TextureCraft operation does not match the current design.".into())
        } else {
            let command = bounded(packet, "command", 64).ok_or("Invalid TextureCraft project operation.")?;
            let args = bounded(packet, "args", 4096).ok_or("Invalid TextureCraft operation arguments.")?;
            let args: Value = serde_json::from_str(&args).map_err(|_| "Invalid TextureCraft operation arguments.")?;
            let fields = args.as_object().ok_or("TextureCraft operation arguments must be an object.")?;
            if command == "document.inspect" && fields.is_empty() {
                texturecraft_engine::execute(&mut app.session, "project.inspect", &args).map_err(|error| error.to_string())
            } else if self.busy() || Self::unfinished(app) {
                Err("Finish the pending TextureCraft project operation first.".into())
            } else {
                let valid = match command.as_str() {
                    "edit.undo" => fields.is_empty(),
                    "layer.add.paint" | "layer.rename" => {
                        fields.len() == 1 && args.get("name").and_then(Value::as_str).is_some_and(|name| !name.trim().is_empty() && name.len() <= 128)
                    }
                    "tool.strokeUv" => {
                        fields.len() == 1
                            && args
                                .get("points")
                                .and_then(Value::as_array)
                                .is_some_and(|points| !points.is_empty() && points.len() <= 128 && points.iter().all(point))
                    }
                    _ => false,
                };
                if valid {
                    texturecraft_engine::execute(&mut app.session, &command, &args).map_err(|error| error.to_string())
                } else {
                    Err("Unsupported TextureCraft project operation.".into())
                }
            }
        };
        match result {
            Ok(value) => reply["result"] = json!(value.to_string()),
            Err(error) => reply["message"] = json!(error),
        }
        self.port.publish("operation-result", reply, None)
    }
}

fn bounded(packet: &JsValue, field: &str, maximum: usize) -> Option<String> {
    string(packet, field).filter(|text| text.len() <= maximum)
}

fn number(packet: &JsValue, field: &str) -> Option<u64> {
    bounded(packet, field, 20)?.parse().ok()
}

fn point(value: &Value) -> bool {
    value.as_array().is_some_and(|axes| axes.len() == 2 && axes.iter().all(|axis| axis.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n))))
}

fn new_project(settings: &Value) -> Result<Session, String> {
    if settings == &json!({"sample":true}) {
        return texturecraft_engine::sample_session(512, 128).map_err(|error| error.to_string());
    }
    let mut session = Session::new_project("Untitled", texturecraft_engine::geom::primitives::material_ball(), "builtin:material_ball", None, 512)
        .map_err(|error| error.to_string())?;
    let Some(fields) = settings.as_object() else { return Err("Invalid TextureCraft project settings.".into()) };
    if fields.is_empty() {
        return Ok(session);
    }
    if fields.len() != 1 {
        return Err("Invalid TextureCraft project settings.".into());
    }
    let options = fields.get("project").ok_or("Invalid TextureCraft project settings.")?;
    let options: NewProject = serde_json::from_value(options.clone()).map_err(|_| "Invalid TextureCraft project settings.")?;
    if options.file.is_some() || options.name.len() > 128 || !matches!(options.resolution, 512 | 1024 | 2048 | 4096) {
        return Err("Invalid TextureCraft project settings.".into());
    }
    let params = json!({"mesh":options.mesh,"resolution":options.resolution,"name":options.name,"uvTiles":options.uv_tiles,"template":options.template});
    texturecraft_engine::execute(&mut session, "project.new", &params).map_err(|error| error.to_string())?;
    if let Some(format) = options.normal_format {
        let id = match format {
            texturecraft_engine::core::NormalFormat::OpenGl => "openGl",
            texturecraft_engine::core::NormalFormat::DirectX => "directX",
        };
        texturecraft_engine::execute(&mut session, "textureSet.settings", &json!({"normalFormat":id})).map_err(|error| error.to_string())?;
        session.modified = false;
    }
    Ok(session)
}

pub(crate) async fn open(port: &AccountPort) -> Result<(Session, Value), String> {
    let request = port.start().await?;
    let session = match (request.bytes, request.import_name) {
        (Some(bytes), Some(name)) => import_device_project(&name, &bytes),
        (Some(bytes), None) => Session::open_account_project(request.path.as_deref(), &bytes, request.recover).map_err(|error| error.to_string()),
        (None, None) => new_project(&request.settings),
        (None, Some(_)) => Err("The TextureCraft import did not include file bytes.".into()),
    }
    .inspect_err(|error| port.open_failed(&request.request_id, error))?;
    let identity = json!({
        "requestId":request.request_id, "documentId":session.project_id.to_string(),
        "revision":session.revision.to_string(), "path":session.path, "ok":true,
    });
    Ok((session, identity))
}

fn import_device_project(name: &str, bytes: &[u8]) -> Result<Session, String> {
    let name = std::path::Path::new(name);
    let extension = name.extension().and_then(|part| part.to_str()).unwrap_or_default();
    if extension.eq_ignore_ascii_case("tcraft") {
        return Session::open_account_project(None, bytes, true).map_err(|error| error.to_string());
    }
    let file = name.to_str().ok_or("Invalid TextureCraft mesh name.")?;
    let title = name.file_stem().and_then(|part| part.to_str()).unwrap_or("Untitled");
    let mut session = Session::from_mesh_bytes(file, bytes, 512, title).map_err(|error| error.to_string())?;
    session.modified = true;
    Ok(session)
}
