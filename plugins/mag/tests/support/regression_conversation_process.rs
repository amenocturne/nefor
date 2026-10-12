//! Process bus for acceptance/lead-turn fixtures. Keep every original MAG
//! emission visible to assertions while servicing canonical conversation barriers.
use nefor_protocol::{Body, PluginOutgoing};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::process::Child;

pub struct ProcessReader {
    stream: BufReader<DuplexStream>,
    pub facts: Vec<Value>,
}
impl std::ops::Deref for ProcessReader {
    type Target = BufReader<DuplexStream>;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}
impl std::ops::DerefMut for ProcessReader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}
impl ProcessReader {
    // Barrier acknowledgements interleave actors. Retain unconsumed facts for
    // other conversations, but return each round's facts only once.
    pub fn facts_for(&mut self, request: &Map<String, Value>) -> Vec<Value> {
        let (selected, pending) =
            std::mem::take(&mut self.facts)
                .into_iter()
                .partition(|fact: &Value| {
                    fact["conversation_id"] == request["conversation_id"]
                        && fact["run_id"] == request["invocation"]["run_id"]
                });
        self.facts = pending;
        selected
    }
}

pub fn attach(
    child: &mut Child,
    translator: &'static str,
    root_conversation: Option<&'static str>,
) -> (DuplexStream, ProcessReader) {
    let mut stdin = child.stdin.take().expect("process stdin");
    let stdout = child.stdout.take().expect("process stdout");
    let (input, input_bus) = tokio::io::duplex(1024 * 1024);
    let (output, mut output_bus) = tokio::io::duplex(1024 * 1024);
    tokio::spawn(async move {
        let mut canonical = crate::canonical_conversation::CanonicalConversation::new(translator);
        let mut root_conversation = root_conversation;
        let mut commands = BufReader::new(input_bus).lines();
        let mut events = BufReader::new(stdout).lines();
        loop {
            tokio::select! {
                command = commands.next_line() => {
                    match command.expect("read fixture command") {
                        Some(line) => {
                            stdin.write_all(line.as_bytes()).await.expect("forward command");
                            stdin.write_all(b"\n").await.expect("command newline");
                            stdin.flush().await.expect("flush command");
                        }
                        None => break,
                    }
                }
                event = events.next_line() => {
                    match event.expect("read process event") {
                        Some(line) => {
                            let outgoing = PluginOutgoing::parse_line(&line).expect("process envelope");
                            if let Body::Event(body) = &outgoing.body {
                                // Lead conversations are normally created by the external agentic-loop,
                                // not by their MAG turn. Seed that authority-owned fact explicitly.
                                if let Some(id) = root_conversation.take() {
                                    let body = json!({"kind":"conversation.fact.append","fact":{
                                        "kind":"created","conversation_id":id,"event_id":format!("{id}:fixture-created")
                                    }})
                                    .as_object()
                                    .unwrap()
                                    .clone();
                                    canonical.service(&body, &mut stdin).await;
                                }
                                canonical.service(body, &mut stdin).await;
                                if body.get("kind").and_then(Value::as_str) == Some("conversation.provider.invoke.request") {
                                    let native = canonical.native_request.take().expect("canonical manager/compositor delivered a native invocation");
                                    assert_eq!(native.get("request_id"), body.get("request_id"), "native invocation correlates the thin request");
                                    assert!(native.get("messages").and_then(Value::as_array).is_some_and(|messages| !messages.is_empty()), "native invocation uses canonical context");
                                }
                            }
                            output_bus.write_all(line.as_bytes()).await.expect("forward process event");
                            output_bus.write_all(b"\n").await.expect("event newline");
                        }
                        None => break,
                    }
                }
            }
        }
    });
    (
        input,
        ProcessReader {
            stream: BufReader::new(output),
            facts: Vec::new(),
        },
    )
}
