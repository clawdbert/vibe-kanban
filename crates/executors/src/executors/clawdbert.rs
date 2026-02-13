use std::{path::Path, process::Stdio, sync::Arc};

use async_trait::async_trait;
use command_group::AsyncCommandGroup;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use crate::{
    command::{CommandBuildError, CommandBuilder, CommandParts, apply_overrides},
    env::ExecutionEnv,
    executors::{AppendPrompt, ExecutorError, SpawnedChild, StandardCodingAgentExecutor},
    logs::utils::EntryIndexProvider,
};

pub mod normalize_logs;

use normalize_logs::normalize_logs;

/// ClawdBert executor configuration.
///
/// ClawdBert is an OpenClaw-based agent executor. Instead of running a coding
/// CLI directly, it dispatches to an OpenClaw gateway session which orchestrates
/// the work — including spawning coding agents, monitoring progress, escalating
/// on failure, and updating task status.
///
/// The base command should point to a dispatch script that accepts the task
/// prompt on stdin and communicates with the OpenClaw gateway.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
pub struct ClawdBert {
    #[serde(default)]
    pub append_prompt: AppendPrompt,

    /// OpenClaw gateway URL (e.g., "http://127.0.0.1:18789")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Gateway URL",
        description = "OpenClaw gateway URL for session dispatch"
    )]
    pub gateway_url: Option<String>,

    /// Maximum number of coding attempts before escalating
    #[serde(default = "default_max_attempts")]
    #[schemars(
        title = "Max Attempts",
        description = "Maximum coding attempts before escalating to the operator"
    )]
    pub max_attempts: u32,

    /// Whether to automatically create a PR on success
    #[serde(default = "default_auto_pr")]
    #[schemars(
        title = "Auto PR",
        description = "Automatically create a pull request on successful completion"
    )]
    pub auto_pr: bool,

    #[serde(flatten)]
    pub cmd: crate::command::CmdOverrides,
}

fn default_max_attempts() -> u32 {
    2
}

fn default_auto_pr() -> bool {
    true
}

impl ClawdBert {
    pub fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        // Resolve dispatch script: env override > scripts/ relative to binary > cwd
        let script = std::env::var("CLAWDBERT_DISPATCH_PATH")
            .ok()
            .filter(|s| !s.is_empty() && std::path::Path::new(s).exists())
            .or_else(|| {
                std::env::current_exe().ok()
                    .and_then(|p| p.parent().map(|d| d.join("../scripts/clawdbert-dispatch.sh")))
                    .filter(|p| p.exists())
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "scripts/clawdbert-dispatch.sh".to_string());

        let builder = CommandBuilder::new(script);

        apply_overrides(builder, &self.cmd)
    }
}

async fn spawn_clawdbert(
    command_parts: CommandParts,
    prompt: &String,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &crate::command::CmdOverrides,
) -> Result<SpawnedChild, ExecutorError> {
    let (program_path, args) = command_parts.into_resolved().await?;

    let mut command = Command::new(program_path);
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(current_dir)
        .args(args);

    env.clone()
        .with_profile(cmd_overrides)
        .apply_to_command(&mut command);

    let mut child = command.group_spawn()?;

    if let Some(mut stdin) = child.inner().stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    Ok(child.into())
}

#[async_trait]
impl StandardCodingAgentExecutor for ClawdBert {
    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let cmd = self.build_command_builder()?.build_initial()?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_clawdbert(cmd, &combined_prompt, current_dir, env, &self.cmd).await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        _session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        // ClawdBert doesn't support session continuity — each attempt is fresh.
        // The taskmaster session on the gateway side tracks state.
        let cmd = self.build_command_builder()?.build_initial()?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_clawdbert(cmd, &combined_prompt, current_dir, env, &self.cmd).await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, current_dir: &Path) {
        normalize_logs(
            msg_store.clone(),
            current_dir,
            EntryIndexProvider::start_from(&msg_store),
        );
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        // ClawdBert doesn't use MCP directly — the underlying coding agent
        // (spawned by the taskmaster) handles MCP configuration.
        None
    }

    fn get_availability_info(&self) -> crate::executors::AvailabilityInfo {
        // The dispatch script lives in scripts/ relative to the VK binary.
        // Also check CLAWDBERT_DISPATCH_PATH env var for overrides.
        let candidates: Vec<std::path::PathBuf> = vec![
            // Env override
            std::env::var("CLAWDBERT_DISPATCH_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_default(),
            // Relative to executable
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("../scripts/clawdbert-dispatch.sh")))
                .unwrap_or_default(),
            // Repo-local fallback
            std::path::PathBuf::from("scripts/clawdbert-dispatch.sh"),
        ];

        for path in &candidates {
            if !path.as_os_str().is_empty() && path.exists() {
                return crate::executors::AvailabilityInfo::InstallationFound;
            }
        }
        crate::executors::AvailabilityInfo::NotFound
    }
}
