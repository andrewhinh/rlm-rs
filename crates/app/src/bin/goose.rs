use goose::prelude::*;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration as StdDuration;
use tokio::time::{Duration as TokioDuration, sleep};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct ReplResponse {
    session_id: String,
    response: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
}

async fn setup_custom_client(user: &mut GooseUser) -> TransactionResult {
    use reqwest::Client;

    let builder = Client::builder()
        .cookie_store(true)
        .gzip(true)
        .timeout(StdDuration::from_secs(60));
    user.set_client_builder(builder).await?;
    Ok(())
}

async fn same_session_roundtrip(user: &mut GooseUser) -> TransactionResult {
    let value = format!("value-{}", Uuid::new_v4());
    let set_payload = json!({
        "code": format!("session_value = \"{value}\""),
    });
    let mut goose = user.post_json("/repl", &set_payload).await?;
    let response = goose
        .response
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let body = response
        .text()
        .await
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let parsed: ReplResponse = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
    };
    if let Some(stderr) = parsed.stderr {
        if !stderr.is_empty() {
            return user.set_failure("stderr not empty", &mut goose.request, None, Some(&body));
        }
    }

    let get_payload = json!({
        "code": "print(session_value)",
    });
    let mut goose = user.post_json("/repl", &get_payload).await?;
    let response = goose
        .response
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let body = response
        .text()
        .await
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let parsed: ReplResponse = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
    };
    let stdout = parsed.stdout.unwrap_or_default();
    if !stdout.contains(&value) {
        return user.set_failure(
            "session value mismatch",
            &mut goose.request,
            None,
            Some(&body),
        );
    }

    Ok(())
}

async fn session_isolation_roundtrip(user: &mut GooseUser) -> TransactionResult {
    let value = format!("isolation-{}", Uuid::new_v4());
    let set_payload = json!({
        "code": format!("session_value = \"{value}\""),
    });
    let mut goose = user.post_json("/repl", &set_payload).await?;
    let response = goose
        .response
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let body = response
        .text()
        .await
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let parsed: ReplResponse = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
    };
    if let Some(stderr) = parsed.stderr {
        if !stderr.is_empty() {
            return user.set_failure("stderr not empty", &mut goose.request, None, Some(&body));
        }
    }

    sleep(TokioDuration::from_millis(50)).await;

    let get_payload = json!({
        "code": "print(session_value)",
    });
    let mut goose = user.post_json("/repl", &get_payload).await?;
    let response = goose
        .response
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let body = response
        .text()
        .await
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let parsed: ReplResponse = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
    };
    let stdout = parsed.stdout.unwrap_or_default();
    if !stdout.contains(&value) {
        return user.set_failure(
            "cross-session leak suspected",
            &mut goose.request,
            None,
            Some(&body),
        );
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), GooseError> {
    GooseAttack::initialize()?
        .register_scenario(
            scenario!("same_session")
                .register_transaction(transaction!(setup_custom_client).set_on_start())
                .register_transaction(transaction!(same_session_roundtrip)),
        )
        .register_scenario(
            scenario!("session_isolation")
                .register_transaction(transaction!(setup_custom_client).set_on_start())
                .register_transaction(transaction!(session_isolation_roundtrip)),
        )
        .execute()
        .await?;
    Ok(())
}
