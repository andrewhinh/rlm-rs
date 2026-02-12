use std::env;
use std::process::{Command, Stdio};

use crate::client::SandboxClient;
use crate::{SandboxHandle, SandboxLaunchConfig, SandboxLauncher};

pub fn build_launcher(config: SandboxLaunchConfig) -> Box<dyn SandboxLauncher> {
    Box::new(DockerRunscLauncher { config })
}

struct DockerRunscLauncher {
    config: SandboxLaunchConfig,
}

impl SandboxLauncher for DockerRunscLauncher {
    fn launch(&self) -> Result<Box<dyn SandboxHandle>, String> {
        let worker_bin = resolve_worker_bin()?;
        let worker_mount = format!("{}:/sandbox_worker:ro", worker_bin.display());
        let mut command = Command::new("docker");
        command
            .arg("run")
            .arg("--rm")
            .arg("-i")
            .arg("--runtime=runsc")
            .arg("-v")
            .arg(worker_mount);
        apply_worker_env_args(&mut command, &self.config);
        command
            .arg("rust:latest")
            .arg("/sandbox_worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let child = command
            .spawn()
            .map_err(|err| format!("failed to spawn sandbox docker container: {err}"))?;
        let mut client = SandboxClient::new(child)?;
        client.ping()?;
        Ok(Box::new(client))
    }
}

fn resolve_worker_bin() -> Result<std::path::PathBuf, String> {
    let current =
        env::current_exe().map_err(|err| format!("failed to resolve current executable: {err}"))?;
    let mut worker = current
        .parent()
        .ok_or_else(|| "failed to resolve executable directory".to_owned())?
        .to_path_buf();
    worker.push("sandbox_worker");
    if let Some(ext) = current.extension() {
        worker.set_extension(ext);
    }
    if !worker.exists() {
        return Err(format!(
            "sandbox worker binary not found at {}. Build it with `cargo build -p app --bin sandbox_worker`",
            worker.display()
        ));
    }
    Ok(worker)
}

fn apply_worker_env_args(command: &mut Command, config: &SandboxLaunchConfig) {
    command
        .arg("-e")
        .arg(format!("OPENAI_API_KEY={}", config.worker.api_key));
}
