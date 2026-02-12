use goose::prelude::*;
use rand::Rng;
use serde_json::json;

const TARGET_CONTEXT_BYTES: usize = 220 * 1024;

async fn setup_custom_client(user: &mut GooseUser) -> TransactionResult {
    use reqwest::Client;

    let builder = Client::builder().cookie_store(true).gzip(true);
    user.set_client_builder(builder).await?;
    Ok(())
}

fn llm_payload(query: &str, context: &str) -> serde_json::Value {
    json!({
        "model": "gpt-5",
        "stream": false,
        "messages": [
            {
                "role": "user",
                "content": format!("Context:\n{context}\n\nQuestion:\n{query}"),
            },
        ],
    })
}

fn generate_massive_context(target_bytes: usize, answer: &str) -> String {
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
    let mut context = String::with_capacity(target_bytes + 1024);
    let mut inserted_answer = false;
    let insertion_point = target_bytes / 2;
    while context.len() < target_bytes {
        if !inserted_answer && context.len() >= insertion_point {
            context.push_str(&format!("The magic number is {answer}\n"));
            inserted_answer = true;
        }
        let num_words = rng.random_range(3..=8);
        let line_words: Vec<&str> = (0..num_words)
            .map(|_| random_words[rng.random_range(0..random_words.len())])
            .collect();
        context.push_str(&line_words.join(" "));
        context.push('\n');
    }

    if !inserted_answer {
        context.push_str(&format!("The magic number is {answer}\n"));
    }
    context
}

async fn llm_roundtrip(user: &mut GooseUser) -> TransactionResult {
    let answer: String = rand::rng().random_range(1_000_000..9_999_999).to_string();
    let context = generate_massive_context(TARGET_CONTEXT_BYTES, &answer);
    let query = "I'm looking for a magic number. What is it?";
    let payload = llm_payload(query, &context);
    let mut goose = user.post_json("/v1/chat/completions", &payload).await?;
    let response = goose
        .response
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(TransactionError::from)
        .map_err(Box::new)?;
    if !status.is_success() {
        return user.set_failure(
            &format!("status {}", status.as_u16()),
            &mut goose.request,
            None,
            Some(&body),
        );
    }
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => return user.set_failure("invalid json", &mut goose.request, None, Some(&body)),
    };
    let content = parsed
        .get("choices")
        .and_then(|value| value.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|value| value.as_str());
    if content.is_none() {
        return user.set_failure("missing content", &mut goose.request, None, Some(&body));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), GooseError> {
    GooseAttack::initialize()?
        .register_scenario(
            scenario!("llm_roundtrip")
                .register_transaction(transaction!(setup_custom_client).set_on_start())
                .register_transaction(transaction!(llm_roundtrip)),
        )
        .execute()
        .await?;
    Ok(())
}
