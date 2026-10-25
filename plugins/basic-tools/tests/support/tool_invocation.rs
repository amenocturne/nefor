fn spawn_tool_invoke(
    out_tx: &mpsc::Sender<PluginOutgoing>,
    body: &Map<String, Value>,
    cancels: &Cancels,
) -> tokio::task::JoinHandle<Result<(), TransportError>> {
    tokio::spawn(tool_invocation(out_tx, body, cancels))
}
