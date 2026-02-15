use rand::Rng;
use std::time::Instant;

use rlm::rlm::{RlmConfig, RlmRepl};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn generate_massive_context(num_lines: usize, answer: &str) -> String {
    println!("Generating massive context with {num_lines} lines");

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
    println!("Magic number inserted at position {magic_position}");

    lines.join("\n")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    println!("Example of using RLM (REPL) with GPT-5-nano on a needle-in-haystack problem.");
    let answer: String = rand::rng().random_range(1_000_000..9_999_999).to_string();
    let answer_for_context = answer.clone();
    let context = tokio::task::spawn_blocking(move || {
        generate_massive_context(1_000_000, &answer_for_context)
    })
    .await?;

    let config = RlmConfig {
        api_key: Some(std::env::var("OPENAI_API_KEY")?),
        base_url: "https://api.openai.com/v1".to_owned(),
        model: "gpt-5".to_owned(),
        recursive_model: "gpt-5-nano".to_owned(),
        depth: 0,
        enable_logging: true,
        max_iterations: 10,
        disable_recursive: false,
    };
    let mut rlm = RlmRepl::new(config)?;
    let query = "I'm looking for a magic number. What is it?";
    let start = Instant::now();
    let result = rlm.completion(context, Some(query)).await?;
    let elapsed = start.elapsed().as_secs_f64();

    println!("Time taken: {elapsed} seconds");
    println!("Result: {result}. Expected: {answer}");
    Ok(())
}
