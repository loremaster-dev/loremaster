use std::time::Duration;

use loremaster_service::{AppState, Config, model_client, router};

/// Leaves room for the reply inside the HTTP API's 30 s limit.
const MODEL_TIMEOUT: Duration = Duration::from_secs(25);

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    lambda_http::tracing::init_default_subscriber();
    let config = Config::from_env()?;
    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let mut dynamo = aws_sdk_dynamodb::config::Builder::from(&aws);
    if let Ok(endpoint) = std::env::var("DYNAMODB_ENDPOINT") {
        dynamo = dynamo.endpoint_url(endpoint);
    }
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) => key,
        Err(_) => {
            let secret = std::env::var("ANTHROPIC_KEY_SECRET")
                .map_err(|_| "set ANTHROPIC_KEY_SECRET or ANTHROPIC_API_KEY")?;
            aws_sdk_secretsmanager::Client::new(&aws)
                .get_secret_value()
                .secret_id(secret)
                .send()
                .await?
                .secret_string
                .ok_or("the API key secret has no string value")?
        }
    };
    let state = AppState::new(
        aws_sdk_dynamodb::Client::from_conf(dynamo.build()),
        model_client(api_key.trim(), MODEL_TIMEOUT),
        config,
    );
    lambda_http::run(router(state)).await
}
