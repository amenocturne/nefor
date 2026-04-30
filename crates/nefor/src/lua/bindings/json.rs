//! `nefor.json` — Rust-backed JSON encode/decode bridged into Lua.
//!
//! Pure-Lua JSON (rxi/json.lua) sat on the hot path for every NCP envelope —
//! 10–40× slower than serde_json for the same workload. With token-grain
//! `chat.stream.delta` events that quickly dominates step latency. This
//! module exposes serde_json through mlua's `serialize` feature so the
//! starter Lua can drop the bundled pure-Lua decoder.
//!
//! Surface:
//! - `nefor.json.encode(value) -> string`
//! - `nefor.json.decode(string) -> value`
//!
//! Both raise a Lua error on failure (invalid JSON, unsupported value)
//! rather than returning `(nil, err)` — callers wrap with `pcall` where
//! they need a typed protocol fault.

use mlua::{Lua, LuaSerdeExt, Table, Value};

/// Install `nefor.json.{encode, decode}` onto `nefor_tbl`.
pub fn install_json(lua: &Lua, nefor_tbl: &Table) -> mlua::Result<()> {
    let json = lua.create_table()?;

    let encode_fn = lua.create_function(|lua, value: Value| {
        let v: serde_json::Value = lua.from_value(value)?;
        serde_json::to_string(&v).map_err(|e| {
            mlua::Error::runtime(format!("nefor.json.encode: {e}"))
        })
    })?;
    json.set("encode", encode_fn)?;

    let decode_fn = lua.create_function(|lua, s: String| {
        let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| {
            mlua::Error::runtime(format!("nefor.json.decode: {e}"))
        })?;
        lua.to_value(&v)
    })?;
    json.set("decode", decode_fn)?;

    nefor_tbl.set("json", json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Lua {
        let lua = Lua::new();
        let nefor = lua.create_table().unwrap();
        install_json(&lua, &nefor).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        lua
    }

    #[test]
    fn encode_object() {
        let lua = setup();
        let s: String = lua
            .load(r#"return nefor.json.encode({ a = 1, b = "x" })"#)
            .eval()
            .unwrap();
        // Field order is not guaranteed across runs; parse and compare.
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["a"], serde_json::json!(1));
        assert_eq!(parsed["b"], serde_json::json!("x"));
    }

    #[test]
    fn encode_array() {
        let lua = setup();
        let s: String = lua
            .load(r#"return nefor.json.encode({ 1, 2, 3 })"#)
            .eval()
            .unwrap();
        assert_eq!(s, "[1,2,3]");
    }

    #[test]
    fn decode_object_roundtrips() {
        let lua = setup();
        let ok: bool = lua
            .load(
                r#"
                local v = nefor.json.decode('{"a":1,"b":"x","c":[1,2,3]}')
                return v.a == 1 and v.b == "x"
                  and v.c[1] == 1 and v.c[2] == 2 and v.c[3] == 3
                "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn roundtrip_preserves_shape() {
        let lua = setup();
        let ok: bool = lua
            .load(
                r#"
                local original = { a = 1, b = "x", c = { 1, 2, 3 } }
                local s = nefor.json.encode(original)
                local v = nefor.json.decode(s)
                return v.a == 1 and v.b == "x"
                  and v.c[1] == 1 and v.c[2] == 2 and v.c[3] == 3
                "#,
            )
            .eval()
            .unwrap();
        assert!(ok);
    }

    #[test]
    fn decode_invalid_json_errors() {
        let lua = setup();
        let err = lua
            .load(r#"nefor.json.decode("{not valid")"#)
            .exec()
            .expect_err("invalid JSON should error");
        assert!(err.to_string().contains("nefor.json.decode"));
    }

    #[test]
    fn encode_nested_envelope() {
        let lua = setup();
        let s: String = lua
            .load(
                r#"
                return nefor.json.encode({
                  type = "system",
                  body = { kind = "ready", protocol_version = "0.1" },
                })
                "#,
            )
            .eval()
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["type"], serde_json::json!("system"));
        assert_eq!(parsed["body"]["kind"], serde_json::json!("ready"));
        assert_eq!(parsed["body"]["protocol_version"], serde_json::json!("0.1"));
    }
}
