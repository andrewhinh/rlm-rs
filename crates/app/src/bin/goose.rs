use goose::prelude::*;
use rand::Rng;
use rlm::prompts::{REPL_SYSTEM_PROMPT, next_action_prompt};
use serde::Deserialize;
use serde_json::json;
// use std::time::Duration as StdDuration;
// use tokio::time::{Duration as TokioDuration, sleep};
// use uuid::Uuid;

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ReplResponse {
    session_id: String,
    response: Option<String>,
    stdout: Option<String>,
    stderr: Option<String>,
}

async fn setup_custom_client(user: &mut GooseUser) -> TransactionResult {
    use reqwest::Client;

    let builder = Client::builder().cookie_store(true).gzip(true);
    user.set_client_builder(builder).await?;
    Ok(())
}

// async fn same_session_roundtrip(user: &mut GooseUser) -> TransactionResult {
//     let value = format!("value-{}", Uuid::new_v4());
//     let set_payload = json!({
//         "code": format!("session_value = \"{value}\""),
//     });
//     let mut goose = user.post_json("/repl", &set_payload).await?;
//     let response = goose
//         .response
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let body = response
//         .text()
//         .await
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let parsed: ReplResponse = match serde_json::from_str(&body) {
//         Ok(parsed) => parsed,
//         Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
//     };
//     if let Some(stderr) = parsed.stderr {
//         if !stderr.is_empty() {
//             return user.set_failure("stderr not empty", &mut goose.request, None, Some(&body));
//         }
//     }

//     let get_payload = json!({
//         "code": "print(session_value)",
//     });
//     let mut goose = user.post_json("/repl", &get_payload).await?;
//     let response = goose
//         .response
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let body = response
//         .text()
//         .await
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let parsed: ReplResponse = match serde_json::from_str(&body) {
//         Ok(parsed) => parsed,
//         Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
//     };
//     let stdout = parsed.stdout.unwrap_or_default();
//     if !stdout.contains(&value) {
//         return user.set_failure(
//             "session value mismatch",
//             &mut goose.request,
//             None,
//             Some(&body),
//         );
//     }

//     Ok(())
// }

// async fn session_isolation_roundtrip(user: &mut GooseUser) -> TransactionResult {
//     let value = format!("isolation-{}", Uuid::new_v4());
//     let set_payload = json!({
//         "code": format!("session_value = \"{value}\""),
//     });
//     let mut goose = user.post_json("/repl", &set_payload).await?;
//     let response = goose
//         .response
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let body = response
//         .text()
//         .await
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let parsed: ReplResponse = match serde_json::from_str(&body) {
//         Ok(parsed) => parsed,
//         Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
//     };
//     if let Some(stderr) = parsed.stderr {
//         if !stderr.is_empty() {
//             return user.set_failure("stderr not empty", &mut goose.request, None, Some(&body));
//         }
//     }

//     sleep(TokioDuration::from_millis(50)).await;

//     let get_payload = json!({
//         "code": "print(session_value)",
//     });
//     let mut goose = user.post_json("/repl", &get_payload).await?;
//     let response = goose
//         .response
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let body = response
//         .text()
//         .await
//         .map_err(TransactionError::from)
//         .map_err(Box::new)?;
//     let parsed: ReplResponse = match serde_json::from_str(&body) {
//         Ok(parsed) => parsed,
//         Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
//     };
//     let stdout = parsed.stdout.unwrap_or_default();
//     if !stdout.contains(&value) {
//         return user.set_failure(
//             "cross-session leak suspected",
//             &mut goose.request,
//             None,
//             Some(&body),
//         );
//     }

//     Ok(())
// }

fn llm_payload(query: &str, context: &str) -> serde_json::Value {
    let user_prompt = next_action_prompt(query, 0, false).content;
    json!({
        "messages": [
            { "role": "system", "content": REPL_SYSTEM_PROMPT },
            { "role": "user", "content": format!("Context:\n{context}") },
            { "role": "user", "content": user_prompt },
        ],
    })
}

fn generate_massive_context(num_lines: usize, answer: &str) -> String {
    let random_words = [
        "blah",
        "random",
        "text",
        "data",
        "content",
        "information",
        "sample",
    ];
    let mut rng = rand::rng();
    let mut lines = Vec::with_capacity(num_lines);
    for _ in 0..num_lines {
        let num_words = rng.random_range(3..=8);
        let line_words: Vec<&str> = (0..num_words)
            .map(|_| random_words[rng.random_range(0..random_words.len())])
            .collect();
        lines.push(line_words.join(" "));
    }

    let magic_position = rng.random_range(400_000..600_000);
    lines[magic_position] = format!("The magic number is {answer}");

    lines.join("\n")
}

async fn llm_roundtrip(user: &mut GooseUser) -> TransactionResult {
    let answer: String = rand::rng().random_range(1_000_000..9_999_999).to_string();
    let context = generate_massive_context(1_000_000, &answer);
    let query = "I'm looking for a magic number. What is it?";
    let payload = llm_payload(query, &context);
    let mut goose = user.post_json("/llm", &payload).await?;
    let response = goose
        .response
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let body = response
        .text()
        .await
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
    };
    let content = parsed.get("content").and_then(|value| value.as_str());
    if content.is_none() {
        return user.set_failure("missing content", &mut goose.request, None, Some(&body));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), GooseError> {
    GooseAttack::initialize()?
        // .register_scenario(
        //     scenario!("same_session")
        //         .register_transaction(transaction!(setup_custom_client).set_on_start())
        //         .register_transaction(transaction!(same_session_roundtrip)),
        // )
        // .register_scenario(
        //     scenario!("session_isolation")
        //         .register_transaction(transaction!(setup_custom_client).set_on_start())
        //         .register_transaction(transaction!(session_isolation_roundtrip)),
        // )
        .register_scenario(
            scenario!("llm_roundtrip")
                .register_transaction(transaction!(setup_custom_client).set_on_start())
                .register_transaction(transaction!(llm_roundtrip)),
        )
        .execute()
        .await?;
    Ok(())
}
