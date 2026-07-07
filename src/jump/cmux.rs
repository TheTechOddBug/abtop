//! cmux (manaflow-ai) backend.
//!
//! Each cmux surface exports `CMUX_WORKSPACE_ID` (a UUID), inherited by the
//! agent process. We read it from the process environment and focus the
//! workspace via the cmux CLI, which accepts the UUID directly as `--workspace`.

use super::{pid_env_var, JumpAttempt, TerminalJumper};
use std::process::Command;

pub struct CmuxJumper;

#[derive(Debug, PartialEq, Eq)]
struct CmuxCommandPlan {
    workspace_id: String,
    terminal_id: Option<String>,
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

impl TerminalJumper for CmuxJumper {
    fn name(&self) -> &'static str {
        "cmux"
    }

    fn try_jump(&self, pid: u32) -> JumpAttempt {
        let Some(plan) = command_plan_from_env(|name| pid_env_var(pid, name)) else {
            return JumpAttempt::NotApplicable;
        };
        match Command::new(&plan.program)
            .args(&plan.args)
            .envs(plan.envs.iter().map(|(key, value)| (key, value)))
            .output()
        {
            Ok(o) if o.status.success() => JumpAttempt::Jumped,
            Ok(o) if command_output_has_broken_pipe(&o.stdout, &o.stderr) => {
                jump_via_applescript_after_socket_failure(&plan)
            }
            Ok(o) => JumpAttempt::Failed(format_command_failure(
                "workspace select",
                &o.status.to_string(),
                &o.stdout,
                &o.stderr,
            )),
            Err(e) => JumpAttempt::Failed(format!("cmux CLI not runnable ({e})")),
        }
    }
}

fn command_plan_from_env(mut env: impl FnMut(&str) -> Option<String>) -> Option<CmuxCommandPlan> {
    let workspace = non_empty(env("CMUX_WORKSPACE_ID"))?;
    let terminal_id = non_empty(env("CMUX_PANEL_ID")).or_else(|| non_empty(env("CMUX_SURFACE_ID")));
    let program = non_empty(env("CMUX_BUNDLED_CLI_PATH")).unwrap_or_else(|| "cmux".to_string());
    let args = vec![
        "workspace".to_string(),
        "select".to_string(),
        workspace.clone(),
    ];
    let envs = ["CMUX_SOCKET_PATH", "CMUX_SOCKET", "CMUX_SOCKET_PASSWORD"]
        .into_iter()
        .filter_map(|name| non_empty(env(name)).map(|value| (name.to_string(), value)))
        .collect();

    Some(CmuxCommandPlan {
        workspace_id: workspace,
        terminal_id,
        program,
        args,
        envs,
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn jump_via_applescript(plan: &CmuxCommandPlan) -> JumpAttempt {
    let script = applescript_focus_script(&plan.workspace_id, plan.terminal_id.as_deref());
    match Command::new("osascript").arg("-e").arg(script).output() {
        Ok(o) if o.status.success() => JumpAttempt::Jumped,
        Ok(o) => JumpAttempt::Failed(format_command_failure(
            "AppleScript focus",
            &o.status.to_string(),
            &o.stdout,
            &o.stderr,
        )),
        Err(e) => JumpAttempt::Failed(format!("AppleScript not runnable ({e})")),
    }
}

fn jump_via_applescript_after_socket_failure(plan: &CmuxCommandPlan) -> JumpAttempt {
    jump_via_applescript_after_socket_failure_with(plan, jump_via_applescript)
}

fn jump_via_applescript_after_socket_failure_with(
    plan: &CmuxCommandPlan,
    jump: impl FnOnce(&CmuxCommandPlan) -> JumpAttempt,
) -> JumpAttempt {
    match jump(plan) {
        JumpAttempt::Failed(msg) if msg.starts_with("AppleScript not runnable") => {
            JumpAttempt::Failed("socket broken; restart cmux".to_string())
        }
        attempt => attempt,
    }
}

fn applescript_focus_script(workspace_id: &str, terminal_id: Option<&str>) -> String {
    let workspace_id = applescript_string(workspace_id);
    let focus_terminal = terminal_id.map(|id| {
        format!(
            "\n        focus (first terminal of w whose id is {})",
            applescript_string(id)
        )
    });

    format!(
        "tell application \"cmux\"\n  repeat with w in windows\n    repeat with candidate in tabs of w\n      if id of candidate is {} then\n        select tab candidate{}\n        activate window w\n        return true\n      end if\n    end repeat\n  end repeat\n  error \"cmux workspace not found\"\nend tell",
        workspace_id,
        focus_terminal.unwrap_or_default()
    )
}

fn applescript_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn format_command_failure(action: &str, status: &str, stdout: &[u8], stderr: &[u8]) -> String {
    if command_output_has_broken_pipe(stdout, stderr) {
        return "socket broken; restart cmux".to_string();
    }

    let mut msg = format!("{action} exited {status}");
    if let Some(detail) = command_output_detail(stderr).or_else(|| command_output_detail(stdout)) {
        msg.push_str(": ");
        msg.push_str(&detail);
    }
    msg
}

fn command_output_contains(output: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(output).contains(needle)
}

fn command_output_has_broken_pipe(stdout: &[u8], stderr: &[u8]) -> bool {
    command_output_contains(stderr, "Broken pipe") || command_output_contains(stdout, "Broken pipe")
}

fn command_output_detail(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let detail = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if detail.is_empty() {
        None
    } else {
        Some(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_failure_includes_stderr_detail() {
        let msg = format_command_failure(
            "workspace select",
            "exit status: 1",
            b"",
            b"Error: workspace not found\n",
        );

        assert_eq!(
            msg,
            "workspace select exited exit status: 1: Error: workspace not found"
        );
    }

    #[test]
    fn command_failure_summarizes_broken_pipe_socket_failure() {
        let msg = format_command_failure(
            "workspace select",
            "exit status: 1",
            b"",
            b"Error: Failed to write to socket (Broken pipe, errno 32)\n",
        );

        assert_eq!(msg, "socket broken; restart cmux");
    }

    #[test]
    fn command_plan_uses_target_cmux_context() {
        let env = |name: &str| match name {
            "CMUX_WORKSPACE_ID" => Some("workspace-1".to_string()),
            "CMUX_PANEL_ID" => Some("terminal-1".to_string()),
            "CMUX_BUNDLED_CLI_PATH" => {
                Some("/Applications/cmux.app/Contents/Resources/bin/cmux".to_string())
            }
            "CMUX_SOCKET_PATH" => Some("/tmp/cmux.sock".to_string()),
            "CMUX_SOCKET_PASSWORD" => Some("pw".to_string()),
            _ => None,
        };

        let plan = command_plan_from_env(env).unwrap();

        assert_eq!(
            plan.program,
            "/Applications/cmux.app/Contents/Resources/bin/cmux"
        );
        assert_eq!(plan.args, ["workspace", "select", "workspace-1"]);
        assert_eq!(
            plan.envs,
            [
                ("CMUX_SOCKET_PATH".to_string(), "/tmp/cmux.sock".to_string()),
                ("CMUX_SOCKET_PASSWORD".to_string(), "pw".to_string()),
            ]
        );
        assert_eq!(plan.workspace_id, "workspace-1");
        assert_eq!(plan.terminal_id.as_deref(), Some("terminal-1"));
    }

    #[test]
    fn applescript_fallback_selects_workspace_and_terminal() {
        let script = applescript_focus_script("workspace-1", Some("terminal-1"));

        assert!(script.contains("if id of candidate is \"workspace-1\" then"));
        assert!(script.contains("select tab candidate"));
        assert!(script.contains("focus (first terminal of w whose id is \"terminal-1\")"));
        assert!(script.contains("activate window w"));
    }

    #[test]
    fn broken_pipe_attempts_applescript_fallback() {
        let plan = CmuxCommandPlan {
            workspace_id: "workspace-1".to_string(),
            terminal_id: Some("terminal-1".to_string()),
            program: "cmux".to_string(),
            args: vec![],
            envs: vec![],
        };
        let mut called = false;

        let result = jump_via_applescript_after_socket_failure_with(&plan, |fallback_plan| {
            called = true;
            assert_eq!(fallback_plan.workspace_id, "workspace-1");
            assert_eq!(fallback_plan.terminal_id.as_deref(), Some("terminal-1"));
            JumpAttempt::Jumped
        });

        assert_eq!(result, JumpAttempt::Jumped);
        assert!(called);
    }
}
