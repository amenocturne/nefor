//! Admission of Lua-owned queued items; semantic rollback remains in Lua.
use nefor_plugin_sdk::{checked_send, TransportError};
use nefor_protocol::PluginOutgoing;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::{engine::Engine, error::TuiError};

fn failure_body(
    body: &Map<String, Value>,
    target: Option<&str>,
    error: &TransportError,
) -> Map<String, Value> {
    let mut correlation = Map::new();
    for key in ["kind", "id", "in_reply_to", "run_id", "chat_id"] {
        if let Some(value) = body
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 512)
        {
            correlation.insert(key.into(), Value::String(value.into()));
        }
    }
    let message = match error {
        TransportError::FrameTooLarge { .. } => "Output admission failed: frame_too_large; this queued item was not published; earlier effects may have occurred.",
        _ => "Output admission failed: invalid_framing; this queued item was not published; earlier effects may have occurred.",
    };
    let mut failure = json!({
        "kind": "tui.delivery_failed", "phase": "admission", "extent": "none",
        "message": message, "correlations": [correlation]
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    if let Some(target) = target.filter(|s| s.len() <= 512) {
        failure.insert("target_hint".into(), Value::String(target.into()));
    }
    failure
}

pub async fn drain_emits_to_writer(
    engine: &mut Engine,
    out: &mpsc::Sender<PluginOutgoing>,
) -> Result<(), TuiError> {
    for (target, body) in engine.take_emit_queue() {
        match checked_send(out, PluginOutgoing::event(body.clone())).await {
            Ok(()) => {}
            Err(
                error @ (TransportError::FrameTooLarge { .. } | TransportError::InvalidFraming),
            ) => {
                if body.get("kind").and_then(Value::as_str) == Some("tui.delivery_failed") {
                    return Err(error.into());
                }
                let failure = failure_body(&body, target.as_deref(), &error);
                // Notify locally even if the observer cannot be admitted. Reducer
                // errors are trusted composition failures, not transport success.
                engine.dispatch_envelope_body(&failure)?;
                checked_send(out, PluginOutgoing::event(failure)).await?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_item_notifies_reducer_and_wire_then_preserves_later_items() {
        let mut engine = Engine::new(80, 24).unwrap();
        engine.load_scenario(r#"
            tui.start {
                initial_state = {}, view = function() return tui.text {content = 'ready'} end,
                update = function(msg, state)
                    if msg.kind == 'tui.delivery_failed' then
                        tui.emit {kind = 'local.observed', extent = msg.extent, id = msg.correlations[1].id}
                    end
                    return state, {}
                end,
            }
            tui.send_to('peer', {kind = 'chat.input.submit', id = 'real-id', text = string.rep('x', 16 * 1024 * 1024)})
            tui.emit {kind = 'later.independent'}
        "#).unwrap();
        let (out, mut incoming) = mpsc::channel(8);
        drain_emits_to_writer(&mut engine, &out).await.unwrap();
        let failure: Value =
            serde_json::from_str(&incoming.recv().await.unwrap().to_line()).unwrap();
        assert_eq!(failure["body"]["kind"], "tui.delivery_failed");
        assert_eq!(failure["body"]["correlations"][0]["id"], "real-id");
        assert_eq!(failure["body"]["target_hint"], "peer");
        assert_eq!(failure["body"]["extent"], "none");
        let later = incoming.recv().await.unwrap().to_line();
        assert!(later.contains("later.independent"));
        drain_emits_to_writer(&mut engine, &out).await.unwrap();
        let local = incoming.recv().await.unwrap().to_line();
        assert!(local.contains("local.observed") && local.contains("real-id"));
        assert!(incoming.try_recv().is_err());
    }

    #[test]
    fn projection_omits_not_truncates_identity_and_target() {
        let body = json!({"kind": "request", "id": "я".repeat(257), "run_id": "valid"});
        let failure = failure_body(
            body.as_object().unwrap(),
            Some(&"x".repeat(513)),
            &TransportError::InvalidFraming,
        );
        let item = &failure["correlations"][0];
        assert!(item.get("id").is_none());
        assert_eq!(item["run_id"], "valid");
        assert!(failure.get("target_hint").is_none());
        assert!(failure["message"].as_str().unwrap().len() <= 512);
    }

    #[tokio::test]
    async fn unavailable_writer_is_terminal_not_a_synthetic_tool_result() {
        let mut engine = Engine::new(80, 24).unwrap();
        engine
            .load_scenario("tui.start {initial_state = {}, view = function() return tui.text {content = 'ready'} end, update = function(m,s) return s,{} end}; tui.emit {kind = 'arbitrary', id = 'not-a-tool'}")
            .unwrap();
        let (out, incoming) = mpsc::channel(1);
        drop(incoming);
        assert!(matches!(
            drain_emits_to_writer(&mut engine, &out).await,
            Err(TuiError::Transport(TransportError::WriterClosed))
        ));
    }
}
