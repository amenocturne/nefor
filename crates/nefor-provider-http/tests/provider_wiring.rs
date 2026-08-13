fn production(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

#[test]
fn every_provider_client_is_built_through_shared_trust() {
    let production_sources = [
        production(include_str!("../../../plugins/openai-provider/src/main.rs")),
        production(include_str!(
            "../../../plugins/chatgpt-provider/src/main.rs"
        )),
        production(include_str!(
            "../../../plugins/chatgpt-provider/src/auth/oauth.rs"
        )),
        production(include_str!(
            "../../../plugins/chatgpt-provider/src/auth/refresh.rs"
        )),
        production(include_str!(
            "../../../plugins/chatgpt-provider/src/responses/mod.rs"
        )),
    ];
    let combined = production_sources.join("\n");

    assert!(!combined.contains("reqwest::Client::new()"));
    assert_eq!(
        combined.matches("reqwest::Client::builder()").count(),
        1,
        "only the explicit local plain-HTTP refresh path may bypass TLS trust construction"
    );
    assert!(
        combined
            .matches("nefor_provider_http::client_builder()")
            .count()
            >= 3
    );
    assert!(combined.matches("nefor_provider_http::client()?").count() >= 2);
}
