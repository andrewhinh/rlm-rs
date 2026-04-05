use std::env;

use rlm::lambda_rlm::LambdaOptions;
use rlm::rlm::RlmMethod;

pub fn string(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

pub fn usize(name: &str, default: usize) -> Result<usize, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|err| format!("invalid {name}: {err}")),
        Err(_) => Ok(default),
    }
}

pub fn f64(name: &str, default: f64) -> Result<f64, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<f64>()
            .map_err(|err| format!("invalid {name}: {err}")),
        Err(_) => Ok(default),
    }
}

pub fn bool(name: &str, default: bool) -> Result<bool, String> {
    match env::var(name) {
        Ok(value) => parse_bool(&value).ok_or_else(|| format!("invalid {name}: {value}")),
        Err(_) => Ok(default),
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("1")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
    {
        return Some(true);
    }
    if value.eq_ignore_ascii_case("0")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("off")
    {
        return Some(false);
    }
    None
}

pub fn lambda_options(defaults: &LambdaOptions) -> Result<LambdaOptions, String> {
    Ok(LambdaOptions {
        context_window_chars: usize(
            "RLM_LAMBDA_CONTEXT_WINDOW_CHARS",
            defaults.context_window_chars,
        )?,
        accuracy_target: f64("RLM_LAMBDA_ACCURACY_TARGET", defaults.accuracy_target)?,
        a_leaf: f64("RLM_LAMBDA_LEAF_ACCURACY", defaults.a_leaf)?,
        a_compose: f64("RLM_LAMBDA_COMPOSE_ACCURACY", defaults.a_compose)?,
    })
}

pub fn method(name: &str, default: RlmMethod) -> Result<RlmMethod, String> {
    match env::var(name) {
        Ok(value) => value.parse(),
        Err(_) => Ok(default),
    }
}
