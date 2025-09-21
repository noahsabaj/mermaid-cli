/// Check if LiteLLM proxy is running
pub async fn is_proxy_running() -> bool {
    let proxy_url = std::env::var("LITELLM_PROXY_URL")
        .unwrap_or_else(|_| "http://localhost:4000".to_string());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(200))  // Much faster timeout
        .build();

    if let Ok(client) = client {
        // Try models endpoint with master key (faster than health endpoint)
        let master_key = std::env::var("LITELLM_MASTER_KEY")
            .unwrap_or_else(|_| "sk-mermaid-1234".to_string());

        if let Ok(resp) = client
            .get(&format!("{}/models", proxy_url))
            .header("Authorization", format!("Bearer {}", master_key))
            .send()
            .await
        {
            return resp.status().is_success();
        }
    }

    false
}