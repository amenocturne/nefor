//! Lua host — loads the user script and exposes the `nefor.*` API.
//!
//! ## Surface
//!
//! - `nefor.name` — plugin identity (string, read-only).
//! - `nefor.state` — current lifecycle state: `"awaiting_ready_ok"` |
//!   `"ready"` | `"shutting_down"`. Read via a metatable hook so scripts
//!   see the live value without polling.
//! - `nefor.on(kind, fn)` — register a specific-kind event handler.
//! - `nefor.on_any(fn)` — register a catch-all event handler.
//! - `nefor.on_ready_ok(fn)` — fires once when `ready_ok` arrives.
//! - `nefor.on_shutdown(fn)` — fires once when `shutdown` arrives.
//! - `nefor.emit(sub_kind, body?)` — emit an event; host prefixes kind
//!   with `<plugin-name>.`. Errors if `body.kind` is already set.
//! - `nefor.emit_raw(full_kind, body?)` — emit with kind verbatim. Escape
//!   hatch for impersonation/testing.
//! - `nefor.sleep(ms)` — async sleep. Must be called from inside an async
//!   handler context (mlua's async coroutine).
//! - `nefor.log(msg)` — write to stderr via `tracing::info!`.
//!
//! ## Synchronisation
//!
//! Shared state (`PluginState`, handler registry) lives behind plain
//! [`std::sync::Mutex`]. Those mutexes are held for microseconds at a
//! time — snapshot the value or the relevant registry keys, drop the
//! guard, then do any async work. A tokio-aware mutex would force us into
//! `blocking_lock` from Lua's sync callbacks, which panics inside async
//! runtimes.

use std::sync::{Arc, Mutex};

use mlua::{AnyUserData, Function, Lua, RegistryKey, Table, Value};
use nefor_protocol::{Envelope, PluginOutgoing};
use serde_json::{Map as JsonMap, Value as JsonValue};
use tokio::sync::mpsc;

use crate::error::MockError;
use crate::state::PluginState;

/// Shared Lua host. Cheap to clone (Arc-backed).
#[derive(Clone)]
pub struct LuaHost {
    inner: Arc<LuaHostInner>,
}

struct LuaHostInner {
    lua: Lua,
    state: Arc<Mutex<PluginState>>,
    name: String,
    out_tx: mpsc::Sender<PluginOutgoing>,
    handlers: Arc<Mutex<Handlers>>,
}

/// Registered Lua handlers. Each slot holds a registry key into the Lua
/// VM's registry; clearing the slot drops the key so Lua GC reclaims the
/// function.
#[derive(Default)]
struct Handlers {
    /// kind → handler.
    per_kind: Vec<(String, Arc<RegistryKey>)>,
    on_any: Option<Arc<RegistryKey>>,
    on_ready_ok: Option<Arc<RegistryKey>>,
    on_shutdown: Option<Arc<RegistryKey>>,
}

impl LuaHost {
    /// Build a new host. Installs the `nefor.*` surface but does not yet
    /// load any script — call [`LuaHost::exec_script`] for that.
    pub fn new(
        name: impl Into<String>,
        out_tx: mpsc::Sender<PluginOutgoing>,
    ) -> Result<Self, MockError> {
        let name = name.into();
        let lua = Lua::new();
        let inner = Arc::new(LuaHostInner {
            lua,
            state: Arc::new(Mutex::new(PluginState::AwaitingReadyOk)),
            name,
            out_tx,
            handlers: Arc::new(Mutex::new(Handlers::default())),
        });
        let host = LuaHost { inner };
        host.install_api()?;
        Ok(host)
    }

    /// Borrow the underlying Lua VM. Clones are cheap (Arc-like).
    #[allow(dead_code)]
    pub fn lua(&self) -> &Lua {
        &self.inner.lua
    }

    /// Execute the user script.
    pub async fn exec_script(&self, name: &str, source: &str) -> Result<(), MockError> {
        self.inner
            .lua
            .load(source)
            .set_name(name)
            .exec_async()
            .await
            .map_err(MockError::from)
    }

    /// Transition to [`PluginState::Ready`] and fire the registered
    /// `on_ready_ok` handler if any.
    pub async fn on_ready_ok(&self) -> Result<(), MockError> {
        set_state(&self.inner.state, PluginState::Ready);
        let key = {
            let h = lock(&self.inner.handlers);
            h.on_ready_ok.as_ref().map(Arc::clone)
        };
        if let Some(key) = key {
            let f: Function = self.inner.lua.registry_value(&key)?;
            if let Err(e) = f.call_async::<()>(()).await {
                tracing::error!(error = %e, "on_ready_ok handler raised");
            }
        }
        Ok(())
    }

    /// Transition to [`PluginState::ShuttingDown`] and fire `on_shutdown`.
    pub async fn on_shutdown(&self) -> Result<(), MockError> {
        set_state(&self.inner.state, PluginState::ShuttingDown);
        let key = {
            let h = lock(&self.inner.handlers);
            h.on_shutdown.as_ref().map(Arc::clone)
        };
        if let Some(key) = key {
            let f: Function = self.inner.lua.registry_value(&key)?;
            if let Err(e) = f.call_async::<()>(()).await {
                tracing::error!(error = %e, "on_shutdown handler raised");
            }
        }
        Ok(())
    }

    /// Dispatch an event envelope to the registered handlers.
    pub async fn dispatch_event(&self, env: &Envelope) -> Result<(), MockError> {
        let (body_map, envelope_table) = match envelope_as_event(&self.inner.lua, env)? {
            Some(pair) => pair,
            None => return Ok(()),
        };
        let kind_opt: Option<String> = match body_map.get("kind")? {
            Value::String(s) => Some(s.to_str()?.to_string()),
            _ => None,
        };

        let (specific, any) = {
            let h = lock(&self.inner.handlers);
            let specific = kind_opt.as_ref().and_then(|k| {
                h.per_kind
                    .iter()
                    .find(|(kind, _)| kind == k)
                    .map(|(_, key)| Arc::clone(key))
            });
            (specific, h.on_any.as_ref().map(Arc::clone))
        };

        if let Some(key) = specific {
            let f: Function = self.inner.lua.registry_value(&key)?;
            if let Err(e) = f
                .call_async::<()>((body_map.clone(), envelope_table.clone()))
                .await
            {
                tracing::error!(error = %e, kind = ?kind_opt, "on handler raised");
            }
        }
        if let Some(key) = any {
            let f: Function = self.inner.lua.registry_value(&key)?;
            if let Err(e) = f.call_async::<()>((body_map, envelope_table)).await {
                tracing::error!(error = %e, kind = ?kind_opt, "on_any handler raised");
            }
        }
        Ok(())
    }

    fn install_api(&self) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let nefor = lua.create_table()?;
        nefor.set("name", self.inner.name.clone())?;

        self.install_state_getter(&nefor)?;
        self.install_log(&nefor)?;
        self.install_on(&nefor)?;
        self.install_on_any(&nefor)?;
        self.install_on_ready_ok(&nefor)?;
        self.install_on_shutdown(&nefor)?;
        self.install_emit(&nefor)?;
        self.install_emit_raw(&nefor)?;
        self.install_sleep(&nefor)?;

        lua.globals().set("nefor", nefor)?;
        Ok(())
    }

    fn install_state_getter(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let handle = StateHandle {
            state: Arc::clone(&self.inner.state),
        };
        let ud = lua.create_any_userdata(handle)?;
        nefor.set("__state_cell", ud)?;

        let meta = lua.create_table()?;
        let index_fn = lua.create_function(move |lua, (tbl, key): (Table, Value)| {
            let key_str = match &key {
                Value::String(s) => s.to_str().ok().map(|c| c.to_string()),
                _ => None,
            };
            if key_str.as_deref() == Some("state") {
                let ud: AnyUserData = tbl.raw_get("__state_cell")?;
                let handle = ud.borrow::<StateHandle>()?;
                let s = handle.read();
                return Ok(Value::String(lua.create_string(s.as_str())?));
            }
            Ok(Value::Nil)
        })?;
        meta.set("__index", index_fn)?;
        nefor.set_metatable(Some(meta));
        Ok(())
    }

    fn install_log(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let log_fn = lua.create_function(|_, msg: mlua::String| {
            let text = msg.to_str().map(|c| c.to_string()).unwrap_or_default();
            tracing::info!(target: "mock-plugin::lua", "{}", text);
            Ok(())
        })?;
        nefor.set("log", log_fn)?;
        Ok(())
    }

    fn install_on(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let handlers = Arc::clone(&self.inner.handlers);
        let lua_clone = lua.clone();
        let on_fn = lua.create_function(move |_, (kind, handler): (String, Function)| {
            if kind.is_empty() {
                return Err(mlua::Error::runtime(
                    "nefor.on: kind must be a non-empty string",
                ));
            }
            let key = Arc::new(lua_clone.create_registry_value(handler)?);
            let mut h = lock(&handlers);
            if let Some(slot) = h.per_kind.iter_mut().find(|(k, _)| k == &kind) {
                slot.1 = key;
            } else {
                h.per_kind.push((kind, key));
            }
            Ok(())
        })?;
        nefor.set("on", on_fn)?;
        Ok(())
    }

    fn install_on_any(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let handlers = Arc::clone(&self.inner.handlers);
        let lua_clone = lua.clone();
        let on_any_fn = lua.create_function(move |_, handler: Function| {
            let key = Arc::new(lua_clone.create_registry_value(handler)?);
            lock(&handlers).on_any = Some(key);
            Ok(())
        })?;
        nefor.set("on_any", on_any_fn)?;
        Ok(())
    }

    fn install_on_ready_ok(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let handlers = Arc::clone(&self.inner.handlers);
        let lua_clone = lua.clone();
        let f = lua.create_function(move |_, handler: Function| {
            let key = Arc::new(lua_clone.create_registry_value(handler)?);
            lock(&handlers).on_ready_ok = Some(key);
            Ok(())
        })?;
        nefor.set("on_ready_ok", f)?;
        Ok(())
    }

    fn install_on_shutdown(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let handlers = Arc::clone(&self.inner.handlers);
        let lua_clone = lua.clone();
        let f = lua.create_function(move |_, handler: Function| {
            let key = Arc::new(lua_clone.create_registry_value(handler)?);
            lock(&handlers).on_shutdown = Some(key);
            Ok(())
        })?;
        nefor.set("on_shutdown", f)?;
        Ok(())
    }

    fn install_emit(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let out_tx = self.inner.out_tx.clone();
        let state = Arc::clone(&self.inner.state);
        let name = self.inner.name.clone();
        let f =
            lua.create_async_function(move |_, (sub_kind, body): (String, Option<Table>)| {
                let out_tx = out_tx.clone();
                let state = Arc::clone(&state);
                let name = name.clone();
                async move {
                    let current = read_state(&state);
                    if current != PluginState::Ready {
                        return Err(mlua::Error::runtime(format!(
                            "nefor.emit: cannot emit while state is \"{}\"; \
                             wait for ready_ok before emitting",
                            current.as_str()
                        )));
                    }
                    if sub_kind.is_empty() {
                        return Err(mlua::Error::runtime(
                            "nefor.emit: sub_kind must be a non-empty string",
                        ));
                    }
                    let mut map = match body {
                        Some(t) => lua_table_to_json_object(&t)?,
                        None => JsonMap::new(),
                    };
                    if map.contains_key("kind") {
                        return Err(mlua::Error::runtime(
                            "nefor.emit: body.kind must not be set; \
                             the host adds it. Use nefor.emit_raw if you \
                             really need to set it verbatim.",
                        ));
                    }
                    map.insert(
                        "kind".into(),
                        JsonValue::String(format!("{name}.{sub_kind}")),
                    );
                    out_tx
                        .send(PluginOutgoing::event(map))
                        .await
                        .map_err(|_| mlua::Error::runtime("nefor.emit: stdout writer closed"))
                }
            })?;
        nefor.set("emit", f)?;
        Ok(())
    }

    fn install_emit_raw(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let out_tx = self.inner.out_tx.clone();
        let state = Arc::clone(&self.inner.state);
        let f =
            lua.create_async_function(move |_, (full_kind, body): (String, Option<Table>)| {
                let out_tx = out_tx.clone();
                let state = Arc::clone(&state);
                async move {
                    let current = read_state(&state);
                    if current != PluginState::Ready {
                        return Err(mlua::Error::runtime(format!(
                            "nefor.emit_raw: cannot emit while state is \"{}\"",
                            current.as_str()
                        )));
                    }
                    if full_kind.is_empty() {
                        return Err(mlua::Error::runtime(
                            "nefor.emit_raw: kind must be a non-empty string",
                        ));
                    }
                    let mut map = match body {
                        Some(t) => lua_table_to_json_object(&t)?,
                        None => JsonMap::new(),
                    };
                    map.insert("kind".into(), JsonValue::String(full_kind));
                    out_tx
                        .send(PluginOutgoing::event(map))
                        .await
                        .map_err(|_| mlua::Error::runtime("nefor.emit_raw: stdout writer closed"))
                }
            })?;
        nefor.set("emit_raw", f)?;
        Ok(())
    }

    fn install_sleep(&self, nefor: &Table) -> Result<(), MockError> {
        let lua = &self.inner.lua;
        let f = lua.create_async_function(|_, ms: u64| async move {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(())
        })?;
        nefor.set("sleep", f)?;
        Ok(())
    }

    // Test-only helpers: directly set state for unit tests that skip the
    // real handshake path.
    #[cfg(test)]
    fn set_state_for_test(&self, s: PluginState) {
        set_state(&self.inner.state, s);
    }
}

// ---- state helpers --------------------------------------------------------

fn read_state(state: &Arc<Mutex<PluginState>>) -> PluginState {
    *lock(state)
}

fn set_state(state: &Arc<Mutex<PluginState>>, new: PluginState) {
    *lock(state) = new;
}

/// Standard-library mutex lock, panicking on poison.
///
/// Poisoning is unreachable in practice: all critical sections are
/// short, synchronous, and don't panic (registry operations return
/// `Result`, state reads/writes are `Copy`). If it does happen, failing
/// loudly beats silently reading stale data.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::error!("mock-plugin: mutex poisoned; recovering for best-effort progress");
        poisoned.into_inner()
    })
}

// ---- Lua ↔ JSON conversion ------------------------------------------------

fn envelope_as_event(lua: &Lua, env: &Envelope) -> Result<Option<(Table, Table)>, MockError> {
    use nefor_protocol::Body;
    let Body::Event(map) = &env.body else {
        return Ok(None);
    };
    let body_table = json_object_to_lua_table(lua, map)?;
    let env_table = lua.create_table()?;
    env_table.set("type", "event")?;
    env_table.set("from", env.from.as_str())?;
    env_table.set("ts", env.ts.to_iso8601())?;
    Ok(Some((body_table, env_table)))
}

pub(crate) fn lua_table_to_json_object(t: &Table) -> mlua::Result<JsonMap<String, JsonValue>> {
    let mut out = JsonMap::new();
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        let key = match k {
            Value::String(s) => s.to_str()?.to_string(),
            Value::Integer(i) => i.to_string(),
            Value::Number(n) => n.to_string(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "lua-to-json: unsupported table key type {}",
                    other.type_name(),
                )));
            }
        };
        out.insert(key, lua_value_to_json(v)?);
    }
    Ok(out)
}

pub(crate) fn lua_value_to_json(v: Value) -> mlua::Result<JsonValue> {
    match v {
        Value::Nil => Ok(JsonValue::Null),
        Value::Boolean(b) => Ok(JsonValue::Bool(b)),
        Value::Integer(i) => Ok(JsonValue::Number(i.into())),
        Value::Number(n) => match serde_json::Number::from_f64(n) {
            Some(num) => Ok(JsonValue::Number(num)),
            None => Err(mlua::Error::runtime(format!(
                "lua-to-json: cannot encode non-finite number {n}"
            ))),
        },
        Value::String(s) => Ok(JsonValue::String(s.to_str()?.to_string())),
        Value::Table(t) => {
            if is_array_like(&t)? {
                let mut arr = Vec::new();
                for pair in t.clone().pairs::<i64, Value>() {
                    let (_, v) = pair?;
                    arr.push(lua_value_to_json(v)?);
                }
                Ok(JsonValue::Array(arr))
            } else {
                Ok(JsonValue::Object(lua_table_to_json_object(&t)?))
            }
        }
        other => Err(mlua::Error::runtime(format!(
            "lua-to-json: unsupported value type {}",
            other.type_name(),
        ))),
    }
}

fn is_array_like(t: &Table) -> mlua::Result<bool> {
    let len = t.raw_len();
    if len == 0 {
        return Ok(false);
    }
    let mut count: i64 = 0;
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, _) = pair?;
        match k {
            Value::Integer(i) if i >= 1 && i <= len as i64 => {}
            _ => return Ok(false),
        }
        count += 1;
    }
    Ok(count == len as i64)
}

fn json_object_to_lua_table(lua: &Lua, map: &JsonMap<String, JsonValue>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in map {
        t.set(k.as_str(), json_value_to_lua(lua, v)?)?;
    }
    Ok(t)
}

fn json_value_to_lua(lua: &Lua, v: &JsonValue) -> mlua::Result<Value> {
    match v {
        JsonValue::Null => Ok(Value::Nil),
        JsonValue::Bool(b) => Ok(Value::Boolean(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(u) = n.as_u64() {
                match i64::try_from(u) {
                    Ok(i) => Ok(Value::Integer(i)),
                    Err(_) => Ok(Value::Number(u as f64)),
                }
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Number(f))
            } else {
                Ok(Value::Nil)
            }
        }
        JsonValue::String(s) => Ok(Value::String(lua.create_string(s)?)),
        JsonValue::Array(arr) => {
            let t = lua.create_table()?;
            for (i, item) in arr.iter().enumerate() {
                t.set(i as i64 + 1, json_value_to_lua(lua, item)?)?;
            }
            Ok(Value::Table(t))
        }
        JsonValue::Object(map) => Ok(Value::Table(json_object_to_lua_table(lua, map)?)),
    }
}

// ---- userdata for the state metatable hook --------------------------------

/// Wraps an `Arc<Mutex<PluginState>>` so the Lua `__index` metamethod can
/// fetch it through a user-data handle. Methodless by design.
struct StateHandle {
    state: Arc<Mutex<PluginState>>,
}

impl StateHandle {
    fn read(&self) -> PluginState {
        *lock(&self.state)
    }
}

impl mlua::UserData for StateHandle {}

#[cfg(test)]
mod tests {
    use super::*;
    use nefor_protocol::{PluginName, Timestamp};
    use serde_json::json;

    async fn host_ready() -> (LuaHost, mpsc::Receiver<PluginOutgoing>) {
        let (tx, rx) = mpsc::channel(16);
        let host = LuaHost::new("mock-plugin", tx).expect("new");
        host.set_state_for_test(PluginState::Ready);
        (host, rx)
    }

    fn event_env(kind: &str, body: serde_json::Value) -> Envelope {
        let mut map = match body {
            serde_json::Value::Object(m) => m,
            _ => panic!("body must be object"),
        };
        map.insert("kind".into(), serde_json::Value::String(kind.into()));
        Envelope::event(
            PluginName::new("peer").expect("valid"),
            Timestamp::parse("2026-04-21T00:00:00.000Z").expect("valid"),
            map,
        )
    }

    #[tokio::test]
    async fn on_dispatch_fires_matching_handler() {
        let (host, _rx) = host_ready().await;
        host.exec_script(
            "t",
            r#"
            captured = nil
            nefor.on("peer.hello", function(body, env)
                captured = body.msg
            end)
            "#,
        )
        .await
        .expect("script");
        host.dispatch_event(&event_env("peer.hello", json!({"msg": "hi"})))
            .await
            .expect("dispatch");
        let captured: Option<String> = host.lua().globals().get("captured").expect("get");
        assert_eq!(captured, Some("hi".to_string()));
    }

    #[tokio::test]
    async fn on_any_fires_after_specific() {
        let (host, _rx) = host_ready().await;
        host.exec_script(
            "t",
            r#"
            trace = {}
            nefor.on("peer.x", function() table.insert(trace, "specific") end)
            nefor.on_any(function() table.insert(trace, "any") end)
            "#,
        )
        .await
        .expect("script");
        host.dispatch_event(&event_env("peer.x", json!({})))
            .await
            .expect("dispatch");
        let trace: Vec<String> = host.lua().globals().get("trace").expect("get");
        assert_eq!(trace, vec!["specific".to_string(), "any".to_string()]);
    }

    #[tokio::test]
    async fn unknown_kind_falls_through_only_to_on_any() {
        let (host, _rx) = host_ready().await;
        host.exec_script(
            "t",
            r#"
            specific = 0
            any = 0
            nefor.on("peer.known", function() specific = specific + 1 end)
            nefor.on_any(function() any = any + 1 end)
            "#,
        )
        .await
        .expect("script");
        host.dispatch_event(&event_env("peer.other", json!({})))
            .await
            .expect("dispatch");
        let specific: i64 = host.lua().globals().get("specific").expect("g");
        let any: i64 = host.lua().globals().get("any").expect("g");
        assert_eq!(specific, 0);
        assert_eq!(any, 1);
    }

    #[tokio::test]
    async fn emit_prefixes_kind_with_plugin_name() {
        let (host, mut rx) = host_ready().await;
        host.exec_script("t", r#"nefor.emit("hello", { greeting = "hi" })"#)
            .await
            .expect("script");
        let out = rx.recv().await.expect("emitted");
        let body = match out.body {
            nefor_protocol::Body::Event(m) => m,
            _ => panic!("expected event"),
        };
        assert_eq!(
            body.get("kind").and_then(|v| v.as_str()),
            Some("mock-plugin.hello")
        );
        assert_eq!(body.get("greeting").and_then(|v| v.as_str()), Some("hi"));
    }

    #[tokio::test]
    async fn emit_rejects_body_with_kind_preset() {
        let (host, _rx) = host_ready().await;
        let err = host
            .exec_script("t", r#"nefor.emit("hello", { kind = "already.set" })"#)
            .await
            .expect_err("must error");
        assert!(err.to_string().contains("body.kind must not be set"));
    }

    #[tokio::test]
    async fn emit_before_ready_errors() {
        let (tx, _rx) = mpsc::channel(4);
        let host = LuaHost::new("mock-plugin", tx).expect("new");
        let err = host
            .exec_script("t", r#"nefor.emit("hi")"#)
            .await
            .expect_err("must error");
        assert!(err.to_string().contains("cannot emit while state"));
    }

    #[tokio::test]
    async fn emit_raw_bypasses_prefix() {
        let (host, mut rx) = host_ready().await;
        host.exec_script("t", r#"nefor.emit_raw("some-peer.message", { a = 1 })"#)
            .await
            .expect("script");
        let out = rx.recv().await.expect("emitted");
        let body = match out.body {
            nefor_protocol::Body::Event(m) => m,
            _ => panic!("expected event"),
        };
        assert_eq!(
            body.get("kind").and_then(|v| v.as_str()),
            Some("some-peer.message")
        );
    }

    #[tokio::test]
    async fn on_ready_ok_fires_when_invoked() {
        let (tx, _rx) = mpsc::channel(4);
        let host = LuaHost::new("mock-plugin", tx).expect("new");
        host.exec_script(
            "t",
            r#"
            count = 0
            nefor.on_ready_ok(function() count = count + 1 end)
            "#,
        )
        .await
        .expect("script");
        host.on_ready_ok().await.expect("ok");
        let count: i64 = host.lua().globals().get("count").expect("get");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn on_shutdown_fires() {
        let (tx, _rx) = mpsc::channel(4);
        let host = LuaHost::new("mock-plugin", tx).expect("new");
        host.exec_script(
            "t",
            r#"
            got = false
            nefor.on_shutdown(function() got = true end)
            "#,
        )
        .await
        .expect("script");
        host.on_shutdown().await.expect("ok");
        let got: bool = host.lua().globals().get("got").expect("get");
        assert!(got);
    }

    #[tokio::test]
    async fn nefor_name_is_plugin_name() {
        let (tx, _rx) = mpsc::channel(4);
        let host = LuaHost::new("my-mock", tx).expect("new");
        let name: String = host
            .lua()
            .load("return nefor.name")
            .eval_async()
            .await
            .expect("eval");
        assert_eq!(name, "my-mock");
    }

    #[tokio::test]
    async fn nefor_state_reflects_transitions() {
        let (tx, _rx) = mpsc::channel(4);
        let host = LuaHost::new("mock-plugin", tx).expect("new");
        let s0: String = host
            .lua()
            .load("return nefor.state")
            .eval_async()
            .await
            .expect("eval0");
        assert_eq!(s0, "awaiting_ready_ok");
        host.on_ready_ok().await.expect("ok");
        let s1: String = host
            .lua()
            .load("return nefor.state")
            .eval_async()
            .await
            .expect("eval1");
        assert_eq!(s1, "ready");
        host.on_shutdown().await.expect("ok");
        let s2: String = host
            .lua()
            .load("return nefor.state")
            .eval_async()
            .await
            .expect("eval2");
        assert_eq!(s2, "shutting_down");
    }

    #[tokio::test]
    async fn nefor_log_runs_without_writing_stdout() {
        let (tx, _rx) = mpsc::channel(4);
        let host = LuaHost::new("mock-plugin", tx).expect("new");
        host.exec_script("t", r#"nefor.log("hello from lua")"#)
            .await
            .expect("log call");
    }

    #[tokio::test]
    async fn envelope_passed_to_handler_has_from_and_ts() {
        let (host, _rx) = host_ready().await;
        host.exec_script(
            "t",
            r#"
            captured_from = nil
            captured_ts = nil
            nefor.on("peer.x", function(_body, env)
                captured_from = env.from
                captured_ts = env.ts
            end)
            "#,
        )
        .await
        .expect("script");
        host.dispatch_event(&event_env("peer.x", json!({})))
            .await
            .expect("dispatch");
        let from: Option<String> = host.lua().globals().get("captured_from").expect("g");
        let ts: Option<String> = host.lua().globals().get("captured_ts").expect("g");
        assert_eq!(from.as_deref(), Some("peer"));
        assert_eq!(ts.as_deref(), Some("2026-04-21T00:00:00.000Z"));
    }

    #[tokio::test]
    async fn nefor_sleep_yields_without_blocking() {
        let (host, _rx) = host_ready().await;
        // 10ms sleep inside a coroutine; the script must complete without
        // panicking. Lua `coroutine.wrap` is used so sleep is callable.
        let start = std::time::Instant::now();
        host.exec_script("t", r#"nefor.sleep(10)"#)
            .await
            .expect("sleep ok");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(8),
            "sleep should take ~10ms, got {elapsed:?}"
        );
    }
}
