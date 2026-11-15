use std::time::Duration;

use lambda_http::{Adapter, LambdaEvent, Service, lambda_runtime, service_fn};
use loremaster_service::{AppState, Config, Event, model_client, purge_account, router};

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
    let mut lambda = aws_sdk_lambda::config::Builder::from(&aws);
    if let Ok(endpoint) = std::env::var("LAMBDA_ENDPOINT") {
        lambda = lambda.endpoint_url(endpoint);
    }
    let mut state = AppState::new(
        aws_sdk_dynamodb::Client::from_conf(dynamo.build()),
        model_client(api_key.trim(), MODEL_TIMEOUT),
        config,
    );
    // The Lambda runtime sets this, so an account purge runs as an async call to this function.
    if let Ok(function_name) = std::env::var("AWS_LAMBDA_FUNCTION_NAME") {
        let lambda = aws_sdk_lambda::Client::from_conf(lambda.build());
        state = state.with_purger(lambda, function_name);
    }
    let http = Adapter::from(router(state.clone()));
    lambda_runtime::run(service_fn(move |event: LambdaEvent<Event>| {
        let state = state.clone();
        let mut http = http.clone();
        async move {
            // A purge replies `null`. Its error makes Lambda retry it.
            match event.payload {
                Event::Purge { user_id } => {
                    purge_account(&state, &user_id).await?;
                    Ok::<_, lambda_http::Error>(serde_json::Value::Null)
                }
                Event::Http(request) => {
                    let response = http.call(LambdaEvent::new(*request, event.context)).await?;
                    Ok(serde_json::to_value(response)?)
                }
            }
        }
    }))
    .await
}
