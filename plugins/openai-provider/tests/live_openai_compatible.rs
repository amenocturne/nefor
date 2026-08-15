use openai_provider::openai::Message;
use openai_provider::stream::run_chat_stream;
use std::env;
use tokio_util::sync::CancellationToken;

const BASE_URL_ENV: &str = "NEFOR_LIVE_OPENAI_BASE_URL";
const API_KEY_ENV: &str = "NEFOR_LIVE_OPENAI_API_KEY";
const MODEL_ENV: &str = "NEFOR_LIVE_OPENAI_MODEL";

fn required_env(name: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("{name} must be set by the explicit live-test recipe"))
}

#[tokio::test]
async fn live_openai_compatible_client_streams_to_a_terminal_outcome() {
    let base_url = required_env(BASE_URL_ENV);
    let api_key = required_env(API_KEY_ENV);
    let model = required_env(MODEL_ENV);
    let endpoint = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let messages = [Message::user(
        "Reply with a short greeting. This is a synthetic protocol test.".to_owned(),
    )];
    let client = reqwest::Client::builder()
        .build()
        .expect("provider HTTP client should initialize");

    let mut streamed_text = String::new();
    let outcome = run_chat_stream(
        &client,
        &endpoint,
        Some(&api_key),
        "Authorization",
        &model,
        &messages,
        None,
        None,
        CancellationToken::new(),
        |delta| streamed_text.push_str(delta),
        |_| {},
    )
    .await
    .expect("live OpenAI-compatible request should succeed");

    assert!(!outcome.interrupted, "live stream must complete normally");
    assert!(
        outcome.finish_reason.is_some(),
        "live stream must report a terminal finish reason"
    );
    assert!(
        !streamed_text.trim().is_empty(),
        "live stream must emit at least one content delta"
    );
    assert_eq!(
        outcome.full_text, streamed_text,
        "stream callbacks and the terminal accumulator must agree"
    );
}
