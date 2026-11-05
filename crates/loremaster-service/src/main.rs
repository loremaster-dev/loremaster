use loremaster_service::{AppState, Config, router};

#[tokio::main]
async fn main() -> Result<(), lambda_http::Error> {
    lambda_http::tracing::init_default_subscriber();
    let config = Config::from_env()?;
    let aws = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let mut dynamo = aws_sdk_dynamodb::config::Builder::from(&aws);
    if let Ok(endpoint) = std::env::var("DYNAMODB_ENDPOINT") {
        dynamo = dynamo.endpoint_url(endpoint);
    }
    let state = AppState::new(aws_sdk_dynamodb::Client::from_conf(dynamo.build()), config);
    lambda_http::run(router(state)).await
}
