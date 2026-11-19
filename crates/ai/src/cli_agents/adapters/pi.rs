//! Pi coding agent (`pi --mode json`).
//!
//! Pi runs one turn per invocation in JSONL event mode (`--mode json`), reading the prompt from
//! stdin so long prompts avoid OS command-line limits (#402). Models and per-model thinking
//! capabilities are discovered via Pi's RPC `get_available_models` query (falling back to
//! `pi --offline --list-models` when needed), and sessions resume with `--session <id>`.

use super::*;
use crate::Effort;
use std::collections::BTreeMap;

pub struct Pi;

fn pi_thinking(effort: Effort) -> &'static str {
    match effort {
        Effort::None => "off",
        Effort::Minimal => "minimal",
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}

fn has_flag(args: &[String], flag: &str, short: Option<&str>) -> bool {
    let prefix = format!("{flag}=");
    args.iter().any(|a| a == flag || a.starts_with(&prefix) || short.is_some_and(|s| a == s || a.starts_with(&format!("{s}="))))
}

fn match_value_flag(arg: &str, names: &[&str]) -> Option<bool> {
    for &name in names {
        if arg == name {
            return Some(true);
        }
        if let Some(rest) = arg.strip_prefix(name)
            && rest.starts_with('=')
        {
            return Some(false);
        }
    }
    None
}

/// Resource-loading flags from `options.args` that affect which provider models `pi` discovers.
/// Run-only flags (`--mode`, `--session`, `--tools`, …) stay out of model discovery.
pub(crate) fn discovery_resource_args(extra: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(arg) = extra.get(i) {
        if let Some(takes_next) = match_value_flag(arg, &["--extension", "-e"]) {
            out.push(arg.clone());
            if takes_next && let Some(val) = extra.get(i + 1) {
                out.push(val.clone());
                i += 1;
            }
        } else if matches!(arg.as_str(), "--no-extensions" | "-ne" | "--approve" | "-a" | "--no-approve" | "-na") {
            out.push(arg.clone());
        }
        i += 1;
    }
    out
}

/// Remove saved flags that could bypass tool, extension, or session limits in restricted runs.
pub(crate) fn strip_restricted_args(extra: &[String], question: bool, no_extensions: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(arg) = extra.get(i) {
        if let Some(takes_next) = match_value_flag(arg, &["--tools", "-t", "--exclude-tools", "-xt"]) {
            if takes_next && extra.get(i + 1).is_some() {
                i += 1;
            }
            i += 1;
            continue;
        }
        if matches!(arg.as_str(), "--no-tools" | "-nt" | "--no-builtin-tools" | "-nbt") {
            i += 1;
            continue;
        }
        if no_extensions {
            if let Some(takes_next) = match_value_flag(arg, &["--extension", "-e"]) {
                if takes_next && extra.get(i + 1).is_some() {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if matches!(arg.as_str(), "--approve" | "-a") {
                i += 1;
                continue;
            }
        }
        if question {
            if let Some(takes_next) = match_value_flag(arg, &["--session", "--session-id", "--fork"]) {
                if takes_next && extra.get(i + 1).is_some() {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if matches!(arg.as_str(), "--continue" | "-c" | "--resume" | "-r") {
                i += 1;
                continue;
            }
        }
        out.push(arg.clone());
        i += 1;
    }
    out
}

fn supported_thinking(reasoning: bool, map: Option<&serde_json::Map<String, Value>>) -> Vec<Effort> {
    if !reasoning {
        return Vec::new();
    }
    Effort::CLI_ALL
        .into_iter()
        .filter(|&effort| {
            let key = pi_thinking(effort);
            let mapped = map.and_then(|m| m.get(key));
            match mapped {
                Some(Value::Null) => false,
                None if matches!(effort, Effort::Xhigh | Effort::Max) => false,
                _ => true,
            }
        })
        .collect()
}

/// Parse Pi RPC `get_available_models` response line into sorted, deduplicated chat models.
pub(crate) fn parse_rpc_models(bytes: &[u8]) -> Result<Option<Vec<models::AgentModel>>, ToolError> {
    if bytes.len() >= 4 * 1024 * 1024 {
        return Err(ToolError::new(ErrorCode::OverLimit, "Pi's model list is too large."));
    }
    let text = String::from_utf8_lossy(bytes);
    let Some(response) = text.lines().find_map(|line| {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        (v.get("type").and_then(Value::as_str) == Some("response") && v.get("command").and_then(Value::as_str) == Some("get_available_models")).then_some(v)
    }) else {
        return Ok(None);
    };
    if response.get("success").and_then(Value::as_bool) != Some(true) {
        let msg = response.get("error").and_then(Value::as_str).unwrap_or("Pi failed to list available models.");
        return Err(ToolError::new(ErrorCode::ProgramFailed, msg));
    }
    let Some(items) = response.pointer("/data/models").and_then(Value::as_array) else {
        return Err(ToolError::new(ErrorCode::GarbledOutput, "Pi returned an unreadable model list."));
    };
    let mut out = BTreeMap::new();
    for item in items {
        if let Some(kind) = item.get("type").and_then(Value::as_str)
            && kind != "chat"
        {
            continue;
        }
        let Some(provider) = item.get("provider").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(model_id) = item.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        let label = item.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(model_id);
        let reasoning = item.get("reasoning").and_then(Value::as_bool).unwrap_or(false);
        let map = item.get("thinkingLevelMap").and_then(Value::as_object);
        let id = format!("{provider}/{model_id}");
        out.insert(id.clone(), models::AgentModel { id, name: format!("{label} ({provider})"), thinking: supported_thinking(reasoning, map) });
    }
    Ok(Some(out.into_values().collect()))
}

fn is_yes_no(s: &str) -> bool {
    s.eq_ignore_ascii_case("yes") || s.eq_ignore_ascii_case("no")
}

/// Parse `pi --offline --list-models` tabular output into sorted, deduplicated models.
/// Only rows after the 6-column table header with valid `yes`/`no` capability flags are accepted,
/// so sign-in guidance text when no models exist yields an empty list rather than bogus entries.
pub(crate) fn parse_list_models(bytes: &[u8]) -> Result<Vec<models::AgentModel>, ToolError> {
    if bytes.len() >= 4 * 1024 * 1024 {
        return Err(ToolError::new(ErrorCode::OverLimit, "Pi's model list is too large."));
    }
    let text = String::from_utf8_lossy(bytes);
    let mut out = BTreeMap::new();
    let mut saw_header = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if !saw_header {
            if let [c0, c1, c2, c3, c4, c5] = cols.as_slice()
                && c0.eq_ignore_ascii_case("provider")
                && c1.eq_ignore_ascii_case("model")
                && c2.eq_ignore_ascii_case("context")
                && c3.eq_ignore_ascii_case("max-out")
                && c4.eq_ignore_ascii_case("thinking")
                && c5.eq_ignore_ascii_case("images")
            {
                saw_header = true;
            }
            continue;
        }
        let [provider, model, _ctx, _max_out, thinking_col, images_col] = cols.as_slice() else {
            continue;
        };
        if provider.is_empty() || model.is_empty() || !is_yes_no(thinking_col) || !is_yes_no(images_col) {
            continue;
        }
        let id = format!("{provider}/{model}");
        // Tabular `--list-models` only reports yes/no; without `thinkingLevelMap`, `xhigh`/`max`
        // cannot be verified per model, so report `off..=high` (the base reasoning depths Pi
        // guarantees for every `reasoning: true` model).
        let thinking =
            if thinking_col.eq_ignore_ascii_case("yes") { vec![Effort::None, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High] } else { Vec::new() };
        out.insert(id.clone(), models::AgentModel { id, name: format!("{model} ({provider})"), thinking });
    }
    Ok(out.into_values().collect())
}

fn run_pi_query(
    adapter: &Pi,
    program: &str,
    host: Option<&str>,
    directory: Option<&Path>,
    raw_args: Vec<String>,
    stdin: Option<&str>,
    stop: &Stop,
) -> Result<craft_anything_platform::Output, ToolError> {
    let mut proc = match host {
        Some(host) => {
            let mut remote = vec![program.to_string()];
            remote.extend(raw_args);
            Process::new("ssh").args(crate::ssh::ssh_args(host, &remote))
        }
        None => {
            let mut p = adapter.process(program).args(raw_args);
            if let Some(dir) = directory {
                p = p.cwd(dir);
            }
            p
        }
    };
    if let Some(input) = stdin {
        proc = proc.stdin_bytes(input);
    }
    let stop2 = stop.clone();
    proc = proc
        .timeout(Duration::from_secs(15))
        .idle_timeout(Duration::from_secs(10))
        .stdout_cap(4 * 1024 * 1024)
        .stderr_cap(64 * 1024)
        .cancel_when(move || stop2.is_stopped())
        .remediation("Check Pi on this computer (`pi --list-models`), then reload the models.");
    proc.run()
}

fn accumulate_usage(p: &mut StreamParser, usage: &Value) {
    let count = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    p.input_tokens = p.input_tokens.saturating_add(count("input")).saturating_add(count("cacheRead")).saturating_add(count("cacheWrite"));
    // Pi's `output` already includes `reasoning` tokens.
    p.output_tokens = p.output_tokens.saturating_add(count("output"));
    if !p.cost_incomplete {
        p.cost_usd = usage.pointer("/cost/total").and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0).and_then(|cost| {
            let total = p.cost_usd.unwrap_or(0.0) + cost;
            total.is_finite().then_some(total)
        });
        if p.cost_usd.is_none() {
            p.cost_incomplete = true;
        }
    }
}

fn assistant_error_text(msg: &Value) -> Option<String> {
    let stop = msg.get("stopReason").and_then(Value::as_str).unwrap_or_default();
    if !matches!(stop, "error" | "aborted" | "length") {
        return None;
    }
    let fallback = match stop {
        "aborted" => "run aborted",
        "length" => "response reached the model's output limit and was cut short",
        _ => "run failed",
    };
    Some(msg.get("errorMessage").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(fallback).to_string())
}

impl AgentAdapter for Pi {
    fn ids(&self) -> &'static [&'static str] {
        &["pi"]
    }

    fn has_model_catalog(&self) -> bool {
        true
    }

    fn model_catalog(
        &self,
        config: &BackendConfig,
        options: &BackendOptions,
        directory: Option<&Path>,
        stop: &Stop,
    ) -> Result<Vec<models::AgentModel>, ToolError> {
        stop.check()?;
        let program = super::super::program_of(config, options)?;
        let host = config.ssh_host.as_deref().map(str::trim).filter(|h| !h.is_empty());
        let resource_args = discovery_resource_args(&options.args);

        let mut rpc_args = vec!["--mode".to_string(), "rpc".to_string(), "--no-session".to_string(), "--offline".to_string()];
        rpc_args.extend(resource_args.iter().cloned());
        if let Ok(out) = run_pi_query(self, &program, host, directory, rpc_args, Some("{\"type\":\"get_available_models\"}\n"), stop)
            && out.success
            && let Some(models) = parse_rpc_models(&out.stdout)?
        {
            return Ok(models);
        }

        let mut list_args = vec!["--offline".to_string(), "--list-models".to_string()];
        list_args.extend(resource_args);
        let out = run_pi_query(self, &program, host, directory, list_args, None, stop)?;
        if !out.success {
            let name = Path::new(&program).file_name().map_or_else(|| program.clone(), |n| n.to_string_lossy().into_owned());
            return Err(craft_anything_platform::process::nonzero_error_with(&name, &out, None));
        }
        parse_list_models(&out.stdout)
    }

    fn validate_model(&self, config: &BackendConfig, options: &BackendOptions, directory: Option<&Path>, stop: &Stop) -> Result<(), ToolError> {
        stop.check()?;
        self.check_native_options(options, directory)?;
        if options.cli_model.is_none() && options.cli_thinking.is_none() {
            return Ok(());
        }
        models::validate(options, &self.model_catalog(config, options, directory, stop)?)
    }

    fn validate_model_for_access(
        &self,
        config: &BackendConfig,
        options: &BackendOptions,
        directory: Option<&Path>,
        access: Access,
        stop: &Stop,
    ) -> Result<(), ToolError> {
        self.validate_access(access)?;
        if matches!(access, Access::Build) {
            return self.validate_model(config, options, directory, stop);
        }
        let mut restricted = options.clone();
        restricted.args = strip_restricted_args(&options.args, matches!(access, Access::Question | Access::CleanRoomQuestion), true);
        if !restricted.args.iter().any(|a| matches!(a.as_str(), "--no-extensions" | "-ne")) {
            restricted.args.push("--no-extensions".into());
        }
        self.validate_model(config, &restricted, directory, stop)
    }

    fn check_native_options(&self, options: &BackendOptions, _cwd: Option<&Path>) -> Result<(), ToolError> {
        if options.cli_model.is_some() && (has_flag(&options.args, "--model", None) || has_flag(&options.args, "--provider", None)) {
            return Err(ToolError::invalid("Choose Pi's model in its model setting, rather than in extra arguments."));
        }
        if options.cli_thinking.is_some() && has_flag(&options.args, "--thinking", None) {
            return Err(ToolError::invalid("Choose Pi's thinking depth in its thinking setting, rather than in extra arguments."));
        }
        if has_flag(&options.args, "--mode", None) {
            return Err(ToolError::invalid("Pi's output mode is set by Craft Anything, so remove --mode from extra arguments."));
        }
        Ok(())
    }

    /// Pi reads project startup prompt/config files and runs its own tools and extensions without
    /// asking Craft Anything first, so Clean room runs cannot enforce Craft Anything's no-read
    /// approval policy.
    fn validate_access(&self, access: Access) -> Result<(), ToolError> {
        match access {
            Access::CleanRoomBuild | Access::CleanRoomAnalyse | Access::CleanRoomQuestion => {
                Err(ToolError::invalid("Pi reads startup files and runs its own tools without asking Craft Anything first, so Clean room can't hold it back.")
                    .with_remediation("Switch to Deep mode to use Pi, or choose an agent that checks reads and edits with Craft Anything."))
            }
            Access::Build | Access::Analyse | Access::Question => Ok(()),
        }
    }

    fn args(&self, cx: &ArgsCx) -> Vec<String> {
        let mut a = vec!["--mode".to_string(), "json".to_string()];
        let restricted = !cx.builder || cx.question || cx.reads_checked;
        if cx.question {
            a.extend(["--no-session".into(), "--no-mcp".into(), "--no-extensions".into()]);
            if cx.reads_checked {
                a.extend(["--no-tools".into(), "--no-context-files".into()]);
            } else {
                a.extend(["--tools".into(), "read,grep,find,ls".into()]);
            }
        } else if !cx.builder {
            a.push("--no-extensions".into());
            if cx.reads_checked {
                a.extend(["--no-session".into(), "--no-mcp".into(), "--no-tools".into(), "--no-context-files".into()]);
            } else {
                a.extend(["--tools".into(), "read,grep,find,ls".into()]);
            }
        }
        if let Some(m) = cx.options.cli_model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            a.extend(["--model".into(), m.to_string()]);
        }
        if let Some(t) = cx.options.cli_thinking {
            a.extend(["--thinking".into(), pi_thinking(t).into()]);
        }
        if let Some(r) = cx.resume.filter(|_| !cx.question) {
            a.extend(["--session".into(), r.to_string()]);
        }
        if restricted {
            a.extend(strip_restricted_args(&cx.options.args, cx.question, true));
        } else {
            a.extend(cx.options.args.iter().cloned());
        }
        a
    }

    fn required_flags(&self) -> (&'static [&'static str], &'static [&'static str]) {
        (&["--help"], &["--mode"])
    }

    fn plain_fallback(&self) -> bool {
        false
    }

    fn streams_json(&self) -> bool {
        true
    }

    fn resumes(&self) -> bool {
        true
    }

    fn questions_fork_session(&self) -> bool {
        false
    }

    fn answers_questions(&self) -> bool {
        true
    }

    fn prompt_on_stdin(&self) -> bool {
        true
    }

    fn stdin_text(&self, preamble: &str, prompt: &str) -> String {
        format!("{preamble}{prompt}")
    }

    fn parse(&self, p: &mut StreamParser, v: &Value) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "session" => {
                if let Some(id) = v.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    p.session_id = Some(id.to_string());
                    p.run_session = Some(id.to_string());
                }
            }
            "agent_start" => {
                p.provisional_error = None;
            }
            "message_update" => {
                if let Some(ev) = v.get("assistantMessageEvent") {
                    match ev.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "text_delta" => {
                            if let Some(delta) = ev.get("delta").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                                out.push(AgentEvent::TextDelta { text: delta.to_string() });
                            }
                        }
                        "thinking_start" => {
                            out.push(AgentEvent::Status { text: "Thinking…".into() });
                        }
                        _ => {}
                    }
                }
            }
            "tool_execution_start" => {
                let id = v.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                let name = v.get("toolName").and_then(Value::as_str).unwrap_or_default();
                let input = v.get("args").cloned().unwrap_or(Value::Null);
                if !id.is_empty() {
                    out.push(p.started(id, name, input));
                }
            }
            "tool_execution_end" => {
                let id = v.get("toolCallId").and_then(Value::as_str).unwrap_or_default();
                let name = v.get("toolName").and_then(Value::as_str).unwrap_or_default();
                if !id.is_empty() {
                    if !p.tool_names.contains_key(id) {
                        out.push(p.started(id, name, Value::Null));
                    }
                    let is_error = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
                    let result = v.get("result").unwrap_or(&Value::Null);
                    let text = result.get("content").map(content_text).unwrap_or_else(|| content_text(result));
                    out.push(p.finished(id, !is_error, &text));
                }
            }
            "turn_end" => {
                if let Some(msg) = v.get("message") {
                    if let Some(usage) = msg.get("usage") {
                        accumulate_usage(p, usage);
                    }
                    if assistant_error_text(msg).is_none() {
                        p.provisional_error = None;
                    }
                }
            }
            "compaction_end" => {
                if let Some(usage) = v.pointer("/result/usage") {
                    accumulate_usage(p, usage);
                }
            }
            "auto_retry_start" => {
                let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(1);
                let max = v.get("maxAttempts").and_then(Value::as_u64).unwrap_or(attempt);
                let why = v.get("errorMessage").and_then(Value::as_str).unwrap_or("retrying");
                out.push(AgentEvent::Status { text: format!("Pi retrying ({attempt}/{max}): {why}") });
            }
            "agent_end" => {
                if v.get("willRetry").and_then(Value::as_bool) != Some(true)
                    && let Some(last) = v
                        .get("messages")
                        .and_then(Value::as_array)
                        .and_then(|ms| ms.iter().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("assistant")))
                {
                    p.provisional_error = assistant_error_text(last);
                }
            }
            "agent_settled" => {
                p.saw_result = true;
                let aborted = v.get("aborted").and_then(Value::as_bool) == Some(true);
                if let Some(why) = p.provisional_error.take().or_else(|| aborted.then(|| "run aborted".to_string())) {
                    p.failure.get_or_insert_with(|| TurnFailure::new(FailureKind::Unknown));
                    out.push(AgentEvent::Error { message: format!("Pi: {why}"), remediation: None, log: None });
                }
            }
            _ => {}
        }
        out
    }

    fn finish(&self, p: &mut StreamParser) -> Vec<AgentEvent> {
        if let Some(why) = p.provisional_error.take() {
            p.failure.get_or_insert_with(|| TurnFailure::new(FailureKind::Unknown));
            return vec![AgentEvent::Error { message: format!("Pi: {why}"), remediation: None, log: None }];
        }
        Vec::new()
    }

    fn parse_stderr(&self, p: &mut StreamParser, line: &str) -> Vec<AgentEvent> {
        if line.to_ascii_lowercase().contains("no session found matching") {
            p.session_missing = true;
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BackendKind;

    fn args(builder: bool, reads_checked: bool, question: bool, resume: Option<&str>, options: &BackendOptions) -> Vec<String> {
        let mcp = McpCommand { command: "craft-anything-cli".into(), args: vec!["mcp".into()], notes: None };
        Pi.args(&ArgsCx { options, prompt: "analyse repo", preamble: "PRE ", mcp: &mcp, resume, builder, reads_checked, question, message_file: None })
    }

    #[test]
    fn pi_args_cover_modes_models_and_sessions() {
        let default_opts = BackendOptions::default();
        assert_eq!(args(true, false, false, None, &default_opts), ["--mode", "json"]);
        assert_eq!(args(false, false, false, None, &default_opts), ["--mode", "json", "--no-extensions", "--tools", "read,grep,find,ls"]);
        assert_eq!(
            args(false, true, false, None, &default_opts),
            ["--mode", "json", "--no-extensions", "--no-session", "--no-mcp", "--no-tools", "--no-context-files"]
        );
        assert_eq!(
            args(false, false, true, Some("sess-1"), &default_opts),
            ["--mode", "json", "--no-session", "--no-mcp", "--no-extensions", "--tools", "read,grep,find,ls"]
        );
        assert_eq!(
            args(false, true, true, Some("sess-1"), &default_opts),
            ["--mode", "json", "--no-session", "--no-mcp", "--no-extensions", "--no-tools", "--no-context-files"]
        );

        let configured = BackendOptions {
            cli_model: Some("opencode-go/deepseek-v4.1-flash".into()),
            cli_thinking: Some(Effort::None),
            args: vec!["--no-extensions".into(), "-t".into(), "read,bash".into()],
            ..Default::default()
        };
        assert_eq!(
            args(true, false, false, Some("sess-1"), &configured),
            ["--mode", "json", "--model", "opencode-go/deepseek-v4.1-flash", "--thinking", "off", "--session", "sess-1", "--no-extensions", "-t", "read,bash"]
        );
        assert!(Pi.resumes() && !Pi.questions_fork_session() && Pi.answers_questions() && Pi.prompt_on_stdin() && Pi.has_model_catalog());
        assert_eq!(Pi.stdin_text("PRE ", "hi"), "PRE hi");
        assert!(AgentAdapters::built_in().for_agent("pi").ids().contains(&"pi"));
    }

    #[test]
    fn pi_restricts_saved_tool_extension_and_session_overrides_outside_builds() {
        let saved = BackendOptions {
            args: vec![
                "--tools".into(),
                "read,bash,write".into(),
                "-t=edit".into(),
                "--exclude-tools".into(),
                "read".into(),
                "-xt=ls".into(),
                "--no-builtin-tools".into(),
                "-nbt".into(),
                "--no-tools".into(),
                "-nt".into(),
                "-e".into(),
                "./custom.ts".into(),
                "--extension=./other.ts".into(),
                "--approve".into(),
                "--session".into(),
                "old-sess".into(),
                "--session-id=123".into(),
                "-c".into(),
                "--offline".into(),
            ],
            ..Default::default()
        };
        assert_eq!(
            args(false, false, true, Some("sess-1"), &saved),
            ["--mode", "json", "--no-session", "--no-mcp", "--no-extensions", "--tools", "read,grep,find,ls", "--offline"]
        );
        assert_eq!(
            args(false, true, true, Some("sess-1"), &saved),
            ["--mode", "json", "--no-session", "--no-mcp", "--no-extensions", "--no-tools", "--no-context-files", "--offline"]
        );
        assert_eq!(
            args(false, false, false, None, &saved),
            ["--mode", "json", "--no-extensions", "--tools", "read,grep,find,ls", "--session", "old-sess", "--session-id=123", "-c", "--offline",]
        );
    }

    #[test]
    fn pi_validates_access_and_forwards_only_discovery_resources() {
        assert!(Pi.validate_access(Access::Build).is_ok());
        assert!(Pi.validate_access(Access::Analyse).is_ok());
        assert!(Pi.validate_access(Access::Question).is_ok());
        assert!(Pi.validate_access(Access::CleanRoomBuild).is_err());
        assert!(Pi.validate_access(Access::CleanRoomAnalyse).is_err());
        assert!(Pi.validate_access(Access::CleanRoomQuestion).is_err());

        let extra = vec![
            "-e".into(),
            "./provider.ts".into(),
            "--extension=./extra.ts".into(),
            "--no-extensions".into(),
            "--session".into(),
            "s1".into(),
            "--mode".into(),
            "json".into(),
            "--tools".into(),
            "read".into(),
            "-a".into(),
        ];
        assert_eq!(discovery_resource_args(&extra), ["-e", "./provider.ts", "--extension=./extra.ts", "--no-extensions", "-a"]);
    }

    #[test]
    fn pi_rejects_conflicting_extra_model_thinking_and_mode_flags() {
        let model_conflict = BackendOptions { cli_model: Some("p/m".into()), args: vec!["--model".into(), "other".into()], ..Default::default() };
        assert!(Pi.check_native_options(&model_conflict, None).is_err());

        let thinking_conflict = BackendOptions { cli_thinking: Some(Effort::High), args: vec!["--thinking=low".into()], ..Default::default() };
        assert!(Pi.check_native_options(&thinking_conflict, None).is_err());

        let mode_conflict = BackendOptions { args: vec!["--mode".into(), "text".into()], ..Default::default() };
        assert!(Pi.check_native_options(&mode_conflict, None).is_err());

        let mode_eq_conflict = BackendOptions { args: vec!["--mode=rpc".into()], ..Default::default() };
        assert!(Pi.check_native_options(&mode_eq_conflict, None).is_err());

        let clean =
            BackendOptions { cli_model: Some("p/m".into()), cli_thinking: Some(Effort::High), args: vec!["--no-extensions".into()], ..Default::default() };
        assert!(Pi.check_native_options(&clean, None).is_ok());
    }

    #[test]
    fn pi_rpc_models_parses_thinking_level_map_and_empty_catalog() {
        let rpc = json!({
            "type": "response",
            "command": "get_available_models",
            "success": true,
            "data": {
                "models": [
                    {"id": "claude-haiku-5-5", "name": "Claude Haiku 5.5", "provider": "opencode-go", "reasoning": true, "type": "chat", "thinkingLevelMap": {"xhigh": "xhigh", "max": "max"}},
                    {"id": "minimax-m3", "name": "MiniMax-M3", "provider": "opencode-go", "reasoning": true, "type": "chat"},
                    {"id": "plain-embed", "name": "Embed", "provider": "local", "reasoning": false, "type": "embedding"},
                    {"id": "plain-chat", "name": "Plain", "provider": "local", "reasoning": false, "type": "chat"}
                ]
            }
        })
        .to_string();
        let models = parse_rpc_models(rpc.as_bytes()).unwrap().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "local/plain-chat");
        assert!(models[0].thinking.is_empty());
        assert_eq!(models[1].id, "opencode-go/claude-haiku-5-5");
        assert_eq!(models[1].thinking, Effort::CLI_ALL);
        assert_eq!(models[2].id, "opencode-go/minimax-m3");
        assert_eq!(models[2].thinking, [Effort::None, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High]);

        let empty = br#"{"type":"response","command":"get_available_models","success":true,"data":{"models":[]}}"#;
        assert!(parse_rpc_models(empty).unwrap().unwrap().is_empty());
    }

    #[test]
    fn pi_list_models_ignores_sign_in_guidance_and_parses_table() {
        let no_models = b"No models available. Use /login to log into a provider via OAuth or API key. See:\n  /docs/providers.md\n  /docs/models.md\n";
        assert!(parse_list_models(no_models).unwrap().is_empty());

        let sample = b"provider     model               context  max-out  thinking  images\nopencode-go  deepseek-v4.1-flash 1M       384K     yes       yes\nlocal        plain-coder         32K      8K       no        no\n";
        let models = parse_list_models(sample).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "local/plain-coder");
        assert!(models[0].thinking.is_empty());
        assert_eq!(models[1].id, "opencode-go/deepseek-v4.1-flash");
        assert_eq!(models[1].thinking, [Effort::None, Effort::Minimal, Effort::Low, Effort::Medium, Effort::High]);
    }

    #[test]
    fn pi_stream_parsing_handles_session_compaction_recovery_and_unrecovered_errors() {
        let adapter: Arc<dyn AgentAdapter> = Arc::new(Pi);
        let mut p = StreamParser::new(adapter.clone());

        assert!(p.line(r#"{"type":"session","version":3,"id":"sess-42","cwd":"/tmp"}"#).is_empty());
        assert_eq!(p.session_id.as_deref(), Some("sess-42"));

        let evs = p.line(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello"}}"#);
        assert!(matches!(evs.as_slice(), [AgentEvent::TextDelta { text }] if text == "Hello"));

        let evs = p.line(r#"{"type":"tool_execution_start","toolCallId":"call_1","toolName":"read","args":{"path":"Cargo.toml"}}"#);
        assert!(matches!(evs.as_slice(), [AgentEvent::ToolStarted { call_id, tool, .. }] if call_id == "call_1" && tool == "read"));

        let evs = p.line(r#"{"type":"tool_execution_end","toolCallId":"call_1","toolName":"read","result":{"content":[{"type":"text","text":"[package]"}]},"isError":false}"#);
        assert!(
            matches!(evs.as_slice(), [AgentEvent::ToolFinished { call_id, tool, ok: true, summary, .. }] if call_id == "call_1" && tool == "read" && summary == "[package]")
        );

        assert!(p.line(r#"{"type":"turn_end","message":{"role":"assistant","stopReason":"stop","usage":{"input":100,"output":25,"cacheRead":20,"cacheWrite":5,"reasoning":10,"cost":{"total":0.015}}}}"#).is_empty());
        assert!(p.line(r#"{"type":"compaction_end","reason":"overflow","result":{"usage":{"input":50,"output":10,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.005}}}}"#).is_empty());
        assert_eq!(p.input_tokens, 175);
        assert_eq!(p.output_tokens, 35);
        assert!((p.cost_usd.unwrap() - 0.020).abs() < 1e-9);

        // Provisional overflow error in agent_end followed by compaction recovery succeeds on settlement.
        assert!(
            p.line(r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","stopReason":"error","errorMessage":"context overflow"}]}"#)
                .is_empty()
        );
        assert!(p.failure.is_none());
        assert!(p.line(r#"{"type":"turn_end","message":{"role":"assistant","stopReason":"stop","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.001}}}}"#).is_empty());
        assert!(p.line(r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","stopReason":"stop"}]}"#).is_empty());
        assert!(p.line(r#"{"type":"agent_settled","aborted":false}"#).is_empty());
        assert!(p.saw_result && p.failure.is_none());

        // Unrecovered error is emitted only when agent_settled confirms failure.
        let mut p2 = StreamParser::new(adapter.clone());
        assert!(
            p2.line(r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","stopReason":"error","errorMessage":"401 unauthorized"}]}"#)
                .is_empty()
        );
        assert!(p2.failure.is_none());
        let evs = p2.line(r#"{"type":"agent_settled","aborted":false}"#);
        assert!(matches!(evs.as_slice(), [AgentEvent::Error { message, .. }] if message == "Pi: 401 unauthorized"));
        assert!(p2.saw_result && p2.failure.is_some());

        // Process exiting cleanly without agent_settled still flushes a pending error.
        let mut p3 = StreamParser::new(adapter.clone());
        assert!(p3.line(r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","stopReason":"aborted"}]}"#).is_empty());
        let evs = p3.finish();
        assert!(matches!(evs.as_slice(), [AgentEvent::Error { message, .. }] if message == "Pi: run aborted"));
        assert!(p3.failure.is_some());

        // Truncated ("length") stopReason fails at settlement if Pi does not recover.
        let mut p4 = StreamParser::new(adapter.clone());
        assert!(p4.line(r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","stopReason":"length"}]}"#).is_empty());
        assert!(p4.failure.is_none());
        let evs = p4.line(r#"{"type":"agent_settled","aborted":false}"#);
        assert!(matches!(evs.as_slice(), [AgentEvent::Error { message, .. }] if message.contains("cut short")));
        assert!(p4.saw_result && p4.failure.is_some());

        // Truncated ("length") cleared if a later turn recovers before settlement.
        let mut p5 = StreamParser::new(adapter);
        assert!(p5.line(r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","stopReason":"length"}]}"#).is_empty());
        assert!(p5.line(r#"{"type":"turn_end","message":{"role":"assistant","stopReason":"stop"}}"#).is_empty());
        assert!(p5.line(r#"{"type":"agent_end","willRetry":false,"messages":[{"role":"assistant","stopReason":"stop"}]}"#).is_empty());
        assert!(p5.line(r#"{"type":"agent_settled","aborted":false}"#).is_empty());
        assert!(p5.saw_result && p5.failure.is_none());

        assert!(Pi.parse_stderr(&mut p, "No session found matching 'sess-missing'").is_empty());
        assert!(p.session_missing);
    }

    #[cfg(unix)]
    #[test]
    fn pi_validates_extension_models_against_the_runs_access_profile() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let prog = dir.path().join("pi");
        let script = r#"#!/bin/sh
cat >/dev/null
for a in "$@"; do
  if [ "$a" = "./ext.ts" ]; then
    echo '{"type":"response","command":"get_available_models","success":true,"data":{"models":[{"id":"base","name":"Base","provider":"p","reasoning":false},{"id":"ext-only","name":"Ext","provider":"p","reasoning":false}]}}'
    exit 0
  fi
done
echo '{"type":"response","command":"get_available_models","success":true,"data":{"models":[{"id":"base","name":"Base","provider":"p","reasoning":false}]}}'
"#;
        std::fs::write(&prog, script).unwrap();
        std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = BackendConfig { name: "Pi".into(), kind: BackendKind::AgentCli, model: "pi".into(), base_url: None, ssh_host: None, enabled: true };
        let options = BackendOptions {
            command: Some(prog.display().to_string()),
            cli_model: Some("p/ext-only".into()),
            args: vec!["-e".into(), "./ext.ts".into()],
            ..Default::default()
        };
        assert!(Pi.validate_model_for_access(&config, &options, None, Access::Build, &Stop::default()).is_ok());
        assert!(Pi.validate_model_for_access(&config, &options, None, Access::Analyse, &Stop::default()).is_err());
        assert!(Pi.validate_model_for_access(&config, &options, None, Access::Question, &Stop::default()).is_err());
        assert!(Pi.validate_model_for_access(&config, &options, None, Access::CleanRoomQuestion, &Stop::default()).is_err());
    }
}
