use goose::prelude::*;
use rand::Rng;
use serde_json::json;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
        "reset": true,
        "messages": [
            {
                "role": "user",
                "content": format!("Context:\n{context}\n\nQuestion:\n{query}"),
            },
        ],
    })
}

fn generate_small_context(num_lines: usize, answer: &str) -> String {
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
    let magic_position = rng.random_range(4_000..6_000);
    lines[magic_position] = format!("The magic number is {answer}");
    lines.join("\n")
}

async fn llm_roundtrip(user: &mut GooseUser) -> TransactionResult {
    let answer: String = rand::rng().random_range(1_000_000..9_999_999).to_string();
    let context = generate_small_context(10_000, &answer);
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
    let Some(content) = content else {
        return user.set_failure("missing content", &mut goose.request, None, Some(&body));
    };
    if !content.contains(&answer) {
        return user.set_failure(
            "incorrect magic number",
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
            scenario!("llm_roundtrip")
                .register_transaction(transaction!(setup_custom_client).set_on_start())
                .register_transaction(transaction!(llm_roundtrip)),
        )
        .execute()
        .await?;
    Ok(())
}
