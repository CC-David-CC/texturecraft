//! The same-origin account transport. Only the owning Desktop frame may answer requests.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use js_sys::{Array, ArrayBuffer, Function, Object, Promise, Reflect, Uint8Array};
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{MessageEvent, Window};

const PREFIX: &str = "texturecraft:account-";
const MAX_BYTES: u32 = 128 << 20;
const REQUEST_TIMEOUT_MS: i32 = 120_000;

type Request = (String, String);

struct Transport {
    parent: Window,
    origin: String,
    generation: String,
    pending: RefCell<BTreeMap<Request, Function>>,
    inbox: RefCell<VecDeque<(String, JsValue)>>,
    ctx: RefCell<Option<eframe::egui::Context>>,
}

struct Listener {
    window: Window,
    callback: Closure<dyn Fn(MessageEvent)>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.window.remove_event_listener_with_callback("message", self.callback.as_ref().unchecked_ref());
    }
}

#[derive(Clone)]
pub(crate) struct AccountPort {
    transport: Rc<Transport>,
    listener: Rc<Listener>,
}

pub(crate) struct Start {
    pub request_id: String,
    pub path: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub recover: bool,
    pub settings: Value,
    pub import_name: Option<String>,
}

pub(crate) fn field(packet: &JsValue, name: &str) -> JsValue {
    Reflect::get(packet, &name.into()).unwrap_or(JsValue::UNDEFINED)
}

pub(crate) fn string(packet: &JsValue, name: &str) -> Option<String> {
    field(packet, name).as_string()
}

fn read_bytes(value: JsValue) -> Result<Vec<u8>, String> {
    let buffer = value.dyn_into::<ArrayBuffer>().map_err(|_| "The account did not send file bytes.".to_owned())?;
    if buffer.byte_length() == 0 || buffer.byte_length() > MAX_BYTES {
        return Err("The account file size is invalid.".into());
    }
    Ok(Uint8Array::new(&buffer).to_vec())
}

impl AccountPort {
    pub fn connect() -> Result<Self, String> {
        let no_parent = || "Open the TextureCraft project from its same-origin WebOS Desktop.".to_owned();
        let window = web_sys::window().ok_or_else(no_parent)?;
        let parent = window.parent().ok().flatten().ok_or_else(no_parent)?;
        let origin = window.origin();
        if Object::is(&parent, &window) || origin == "null" || parent.location().origin().ok().as_deref() != Some(origin.as_str()) {
            return Err(no_parent());
        }
        let mut random = [0; 16];
        window.crypto().and_then(|crypto| crypto.get_random_values_with_u8_array(&mut random)).map_err(|_| "Could not create an account window identity.")?;
        let generation = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let transport = Rc::new(Transport { parent, origin, generation, pending: RefCell::default(), inbox: RefCell::default(), ctx: RefCell::default() });
        let owner = Rc::downgrade(&transport);
        let callback = Closure::<dyn Fn(MessageEvent)>::new(move |event: MessageEvent| {
            if let Some(owner) = owner.upgrade() {
                owner.receive(event);
            }
        });
        window.add_event_listener_with_callback("message", callback.as_ref().unchecked_ref()).map_err(|_| "Could not listen to the WebOS account window.")?;
        Ok(Self { transport, listener: Rc::new(Listener { window, callback }) })
    }

    fn post(&self, kind: &str, fields: &[(&str, JsValue)], transfer: Option<&ArrayBuffer>) -> Result<(), String> {
        let packet = Object::new();
        for (name, value) in fields {
            Reflect::set(&packet, &(*name).into(), value).map_err(|_| "Could not make the account request.")?;
        }
        self.post_object(kind, packet.as_ref(), transfer)
    }

    fn post_object(&self, kind: &str, packet: &JsValue, transfer: Option<&ArrayBuffer>) -> Result<(), String> {
        Reflect::set(packet, &"type".into(), &format!("{PREFIX}{kind}").into()).map_err(|_| "Could not make the account request.")?;
        Reflect::set(packet, &"generation".into(), &self.transport.generation.as_str().into()).map_err(|_| "Could not make the account request.")?;
        let parent = &self.transport.parent;
        let origin = &self.transport.origin;
        let sent = match transfer {
            Some(buffer) => parent.post_message_with_transfer(packet, origin, &Array::of1(buffer)),
            None => parent.post_message(packet, origin),
        };
        sent.map_err(|_| "Could not reach the account window.".into())
    }

    pub fn publish(&self, kind: &str, data: Value, bytes: Option<Vec<u8>>) -> Result<(), String> {
        let packet = js_sys::JSON::parse(&data.to_string()).map_err(|_| "Could not encode the account notice.")?;
        let transfer = bytes.map(|bytes| Uint8Array::from(bytes.as_slice()).buffer());
        if let Some(buffer) = &transfer {
            Reflect::set(&packet, &"bytes".into(), buffer.as_ref()).map_err(|_| "Could not encode the account file.")?;
        }
        self.post_object(kind, &packet, transfer.as_ref())
    }

    pub fn wake_on(&self, ctx: eframe::egui::Context) {
        *self.transport.ctx.borrow_mut() = Some(ctx);
    }

    pub fn take_messages(&self) -> VecDeque<(String, JsValue)> {
        std::mem::take(&mut *self.transport.inbox.borrow_mut())
    }

    fn wait(&self, key: &Request) -> Result<(JsFuture, i32, Closure<dyn FnMut()>), String> {
        let mut reject = None;
        let promise = Promise::new(&mut |resolve, failed| {
            self.transport.pending.borrow_mut().insert(key.clone(), resolve);
            reject = Some(failed);
        });
        let owner = Rc::downgrade(&self.transport);
        let request = key.clone();
        let timeout = reject.ok_or("Could not schedule an account request.")?;
        let callback = Closure::once(move || {
            if owner.upgrade().is_some_and(|owner| owner.pending.borrow_mut().remove(&request).is_some()) {
                let _ = timeout.call1(&JsValue::UNDEFINED, &"The account window did not answer.".into());
            }
        });
        let timer = self.listener.window.set_timeout_with_callback_and_timeout_and_arguments_0(callback.as_ref().unchecked_ref(), REQUEST_TIMEOUT_MS);
        match timer {
            Ok(timer) => Ok((JsFuture::from(promise), timer, callback)),
            Err(_) => {
                self.transport.pending.borrow_mut().remove(key);
                Err("Could not start an account request.".into())
            }
        }
    }

    async fn exchange(&self, key: Request, outgoing: &str, fields: &[(&str, JsValue)], transfer: Option<&ArrayBuffer>) -> Result<JsValue, String> {
        let (reply, timer, callback) = self.wait(&key)?;
        let response = match self.post(outgoing, fields, transfer) {
            Ok(()) => reply.await.map_err(|error| error.as_string().unwrap_or_else(|| "The account request failed.".into())),
            Err(error) => Err(error),
        };
        self.listener.window.clear_timeout_with_handle(timer);
        self.transport.pending.borrow_mut().remove(&key);
        drop(callback);
        response
    }

    pub async fn start(&self) -> Result<Start, String> {
        let packet = self.exchange(("start".into(), String::new()), "ready", &[], None).await?;
        let request_id = string(&packet, "requestId").ok_or("Missing account Open request identity.")?;
        let parsed = (|| {
            let path = field(&packet, "path");
            let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid account project path.")?) };
            let recover = field(&packet, "recover").as_bool().ok_or("Invalid account recovery request.")?;
            let content = field(&packet, "bytes");
            let bytes = if content.is_null() { None } else { Some(read_bytes(content)?) };
            let imported = field(&packet, "importName");
            let import_name = if imported.is_null() {
                None
            } else {
                let name = imported.as_string().ok_or("Invalid TextureCraft import name.")?;
                if name.len() > 255
                    || name.trim().is_empty()
                    || matches!(name.as_str(), "." | "..")
                    || name.chars().any(|ch| ch == '/' || ch == '\\' || ch.is_control())
                    || path.is_some()
                    || recover
                    || bytes.is_none()
                {
                    return Err("Invalid TextureCraft account import.".into());
                }
                Some(name)
            };
            if bytes.is_none() && (path.is_some() || recover) {
                return Err("Invalid TextureCraft account project request.".to_owned());
            }
            let settings = field(&packet, "newSettings");
            let settings = if settings.is_null() {
                serde_json::json!({})
            } else {
                let text = settings.as_string().filter(|value| value.len() <= 4096).ok_or("Invalid TextureCraft project settings.")?;
                serde_json::from_str::<Value>(&text).map_err(|_| "Invalid TextureCraft project settings.")?
            };
            if !settings.is_object() || import_name.is_some() && settings.as_object().is_some_and(|fields| !fields.is_empty()) {
                return Err("Invalid TextureCraft project settings.".into());
            }
            Ok(Start { request_id: request_id.clone(), path, bytes, recover, settings, import_name })
        })();
        if let Err(error) = &parsed {
            self.open_failed(&request_id, error);
        }
        parsed
    }

    pub fn open_failed(&self, request_id: &str, error: &str) {
        let _ = self.post("started", &[("requestId", request_id.into()), ("ok", false.into()), ("message", error.into())], None);
    }

    pub fn startup_failed(&self, error: &str) {
        let _ = self.post("failed", &[("message", error.into())], None);
    }
}

impl Transport {
    fn receive(&self, event: MessageEvent) {
        if event.origin() != self.origin || !event.source().is_some_and(|source| Object::is(&source, &self.parent)) {
            return;
        }
        let packet = event.data();
        if !packet.is_object() || string(&packet, "generation").as_deref() != Some(self.generation.as_str()) {
            return;
        }
        let kind = string(&packet, "type");
        let request = match kind.as_deref().and_then(|kind| kind.strip_prefix(PREFIX)) {
            Some("start") => ("start".into(), String::new()),
            Some(kind @ ("recovery-capture" | "session-save" | "save-result" | "session" | "operation" | "dialog-result")) => {
                if let Some(ctx) = self.ctx.borrow().as_ref()
                    && let Ok(mut inbox) = self.inbox.try_borrow_mut()
                    && inbox.len() < 32
                {
                    inbox.push_back((kind.to_owned(), packet));
                    ctx.request_repaint();
                }
                return;
            }
            _ => return,
        };
        let resolve = self.pending.borrow_mut().remove(&request);
        if let Some(resolve) = resolve {
            let _ = resolve.call1(&JsValue::UNDEFINED, &packet);
        }
    }
}
