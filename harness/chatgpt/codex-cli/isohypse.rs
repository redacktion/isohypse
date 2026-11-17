use codex_protocol::items::CommandExecutionItem;
use codex_protocol::items::CommandExecutionStatus;
use codex_protocol::items::FileChangeItem;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyStatus;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use codex_core::config::Config;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ThreadStopInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_tools::ResponsesApiTool;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::sync::Mutex;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeConfig {
    node: PathBuf,
    worker: PathBuf,
    adapter_config: PathBuf,
    schema: PathBuf,
}

struct Extension {
    config: NativeConfig,
    spec: ToolSpec,
}

struct Runtime {
    config: NativeConfig,
    spec: ToolSpec,
    workspace: PathBuf,
    thread_id: String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    transport: Option<Transport>,
    in_flight: bool,
    closed: bool,
    stopped: bool,
    seen: HashSet<String>,
}

struct Transport {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

/// Registers a server-owned native tool for every connection, including remote
/// control. Only the operator-selected config enables this extension.
pub(crate) fn install(builder: &mut ExtensionRegistryBuilder<Config>) {
    let Some(path) = std::env::var_os("ISOHYPSE_NATIVE_CONFIG") else {
        return;
    };
    let loaded = (|| -> anyhow::Result<Extension> {
        let config: NativeConfig = serde_json::from_slice(&std::fs::read(path)?)?;
        for path in [
            &config.node,
            &config.worker,
            &config.adapter_config,
            &config.schema,
        ] {
            anyhow::ensure!(
                path.is_absolute() && path.is_file(),
                "Invalid native Isohypse path"
            );
        }
        let schema: Value = serde_json::from_slice(&std::fs::read(&config.schema)?)?;
        let spec = ToolSpec::Function(ResponsesApiTool {
            name: "isohypse".into(),
            description: schema["description"]
                .as_str()
                .unwrap_or("Read and edit this workspace through Isohypse")
                .into(),
            parameters: serde_json::from_value(schema["inputSchema"].clone())?,
            strict: false,
            defer_loading: None,
            output_schema: None,
        });
        Ok(Extension { config, spec })
    })();
    match loaded {
        Ok(extension) => {
            let extension = Arc::new(extension);
            builder.thread_lifecycle_contributor(extension.clone());
            builder.turn_lifecycle_contributor(extension.clone());
            builder.tool_contributor(extension);
        }
        Err(error) => tracing::error!(%error, "Native Isohypse configuration rejected"),
    }
}

impl ThreadLifecycleContributor<Config> for Extension {
    fn on_thread_start<'a>(
        &'a self,
        input: ThreadStartInput<'a, Config>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input.thread_store.insert(Runtime {
                config: self.config.clone(),
                spec: self.spec.clone(),
                workspace: input.config.cwd.as_path().to_path_buf(),
                thread_id: input.thread_store.level_id().to_string(),
                state: Mutex::new(State::default()),
            });
        })
    }

    fn on_thread_stop<'a>(&'a self, input: ThreadStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(runtime) = input.thread_store.get::<Runtime>() {
                runtime.close().await;
            }
        })
    }
}

impl TurnLifecycleContributor for Extension {
    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(runtime) = input.thread_store.get::<Runtime>() {
                runtime.abort().await;
            }
        })
    }
}

impl ToolContributor for Extension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        match thread_store.get::<Runtime>() {
            Some(runtime) => vec![runtime],
            None => Vec::new(),
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for Runtime {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("isohypse")
    }
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn handle<'a>(&'a self, invocation: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let arguments = invocation.function_arguments()?;
            // Matches the daemon's MAX_REQUEST_BYTES; a lower client cap refuses
            // legitimate authoring before the daemon ever sees it.
            if arguments.len() > 8 * 1024 * 1024 - 65536 {
                return Err(FunctionCallError::RespondToModel(
                    "Isohypse batch exceeds the 8 MiB limit; split it across batches".into(),
                ));
            }
            // Mobile control of this machine is local execution. A remote
            // execution environment must never silently reach the local daemon.
            if let Some(environment) = invocation.environments.first()
                && (environment.environment_id != "local"
                    || environment
                        .cwd
                        .to_abs_path()
                        .map_or(true, |cwd| cwd.as_path() != self.workspace))
            {
                return Err(FunctionCallError::RespondToModel(
                    "Isohypse requires this thread's local workspace; environment changed".into(),
                ));
            }
            let arguments: Value = serde_json::from_str(arguments)
                .map_err(|e| FunctionCallError::RespondToModel(e.to_string()))?;
            let command = Self::command(&arguments);
            let request = json!({"callId":invocation.call_id,"turnId":invocation.turn_id,"arguments":arguments});
            let mut activity = self.command_item(&invocation.call_id, command);
            invocation
                .turn_item_emitter
                .emit_command_execution(activity.clone())
                .await;
            let mut result = match self.run(request).await {
                Ok(result) => result,
                Err(error) => {
                    activity.status = CommandExecutionStatus::Failed;
                    activity.stderr = Some(error.to_string());
                    activity.aggregated_output = activity.stderr.clone();
                    activity.exit_code = Some(1);
                    invocation
                        .turn_item_emitter
                        .emit_command_execution(activity)
                        .await;
                    return Err(FunctionCallError::RespondToModel(error.to_string()));
                }
            };
            let success = result["success"].as_bool().unwrap_or(false);
            self.publish_changes(
                &invocation.call_id,
                invocation.turn_item_emitter.as_ref(),
                &mut result,
            )
            .await;
            let output = result["output"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| result.to_string());
            activity.status = if success {
                CommandExecutionStatus::Completed
            } else {
                CommandExecutionStatus::Failed
            };
            activity.stdout = success.then_some(output.clone());
            activity.stderr = (!success).then_some(output.clone());
            activity.aggregated_output = Some(output);
            activity.exit_code = Some(if success { 0 } else { 1 });
            invocation
                .turn_item_emitter
                .emit_command_execution(activity)
                .await;
            Ok(
                Box::new(JsonToolOutput::with_success(result, Some(success)))
                    as Box<dyn ToolOutput>,
            )
        })
    }
}

impl Runtime {
    fn command(arguments: &Value) -> Vec<String> {
        let mut command = vec!["isohypse".to_string()];
        command.extend(
            arguments["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|step| step["op"].as_str())
                .take(4)
                .map(str::to_owned),
        );
        command
    }

    fn command_item(&self, call_id: &str, command: Vec<String>) -> CommandExecutionItem {
        CommandExecutionItem {
            sandbox_type: None,
            model_context: None,
            id: format!("{call_id}-isohypse"),
            plugin_id: None,
            script_path: None,
            process_id: None,
            command,
            cwd: AbsolutePathBuf::from_absolute_path(self.workspace.clone())
                .expect("thread workspace is absolute")
                .into(),
            parsed_cmd: Vec::new(),
            source: ExecCommandSource::Agent,
            interaction_input: None,
            status: CommandExecutionStatus::InProgress,
            stdout: None,
            stderr: None,
            aggregated_output: None,
            exit_code: None,
            duration: None,
            formatted_output: None,
        }
    }

    async fn run(&self, request: Value) -> anyhow::Result<Value> {
        let mut state = self.state.lock().await;
        anyhow::ensure!(!state.stopped, "Isohypse thread closed; no replay");
        let reconnect = request["arguments"] == json!({"steps":[{"op":"session.reconnect"}]});
        let recovering = reconnect && (state.closed || state.in_flight);
        anyhow::ensure!(
            reconnect || (!state.closed && !state.in_flight),
            "Isohypse outcome unconfirmed; call session.reconnect alone, then inspect affected files; no replay"
        );
        let call_id = request["callId"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing call ID"))?;
        anyhow::ensure!(
            state.seen.len() < 4096,
            "Isohypse thread call limit reached"
        );
        anyhow::ensure!(
            state.seen.insert(call_id.to_string()),
            "Duplicate Isohypse call; no replay"
        );
        if recovering {
            if let Some(transport) = state.transport.take() {
                transport.drain().await;
            }
            state.closed = false;
            state.in_flight = false;
        }
        if state.transport.is_none() {
            let mut child = Command::new(&self.config.node)
                .arg(&self.config.worker)
                .arg(&self.workspace)
                .arg(&self.config.adapter_config)
                .arg(&self.thread_id)
                .current_dir(&self.workspace)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()?;
            let input = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("Missing worker stdin"))?;
            let output = BufReader::new(
                child
                    .stdout
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("Missing worker stdout"))?,
            );
            state.transport = Some(Transport {
                child,
                input,
                output,
            });
        }
        // If the future is cancelled, this remains set: never consume a late
        // response as the next call's answer or replay a potentially applied edit.
        state.in_flight = true;
        let transport = state
            .transport
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Missing worker"))?;
        let result = tokio::time::timeout(Duration::from_secs(660), async {
            transport.input.write_all(format!("{request}\n").as_bytes()).await?;
            transport.input.flush().await?;
            let mut line = Vec::new();
            (&mut transport.output).take(16 * 1024 * 1024).read_until(b'\n', &mut line).await?;
            anyhow::ensure!(line.last() == Some(&b'\n') && line.len() < 16 * 1024 * 1024, "Native worker disconnected or exceeded response limit; outcome unconfirmed; no replay");
            Ok::<Value, anyhow::Error>(serde_json::from_slice(&line)?)
        }).await;
        match result {
            Ok(Ok(mut value)) => {
                state.in_flight = false;
                if recovering {
                    let output = value["output"].as_str().unwrap_or_default();
                    value["output"] = json!(format!(
                        "{output}\nPrior call outcome remains unconfirmed. Nothing was replayed; inspect affected files before another mutation."
                    ));
                }
                Ok(value)
            }
            Ok(Err(error)) => Err(error),
            Err(error) => Err(error.into()),
        }
    }

    async fn abort(&self) {
        // A turn can be interrupted while Isohypse is idle. Its completed calls
        // are confirmed and the thread-owned session remains usable next turn.
        // Only an interrupted request poisons this transport: never replay it.
        let mut state = self.state.lock().await;
        if !state.in_flight {
            return;
        }
        state.closed = true;
        if let Some(transport) = state.transport.take() {
            transport.drain().await;
        }
    }

    async fn close(&self) {
        self.state.lock().await.stopped = true;
        self.close_transport().await;
    }

    async fn close_transport(&self) {
        let mut state = self.state.lock().await;
        state.closed = true;
        if let Some(transport) = state.transport.take() {
            transport.drain().await;
        }
    }
}

impl Transport {
    async fn drain(mut self) {
        let drained = tokio::time::timeout(Duration::from_secs(35), async {
            self.input.write_all(b"{\"close\":true}\n").await?;
            self.input.shutdown().await?;
            self.child.wait().await
        })
        .await;
        if !matches!(drained, Ok(Ok(status)) if status.success()) {
            tracing::error!("Isohypse drain failed; outcome unconfirmed; inspect before retrying");
        }
    }
}

#[cfg(test)]
#[path = "isohypse_tests.rs"]
mod tests;

impl Runtime {
    async fn publish_changes(
        &self,
        call_id: &str,
        emitter: &dyn codex_tools::TurnItemEmitter,
        result: &mut Value,
    ) {
        // Only confirmed transactions publish edits. Failed validation/rollback
        // and interrupted transports must never appear as completed file changes.
        let mutations = result
            .as_object_mut()
            .and_then(|map| map.remove("mutations"));
        if result["success"] != true {
            return;
        }
        let mut missing_diff = false;
        for (step, mutation) in mutations
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let report = &mutation["json"];
            let mut files = Vec::new();
            match mutation["op"].as_str() {
                Some("mutate.create") => {
                    if let (Some(path), Some(content)) =
                        (report["path"].as_str(), report["content"].as_str())
                    {
                        files.push((
                            path,
                            FileChange::Add {
                                content: content.into(),
                            },
                        ));
                    } else {
                        missing_diff = true;
                    }
                }
                Some("mutate.edit") => {
                    for section in report["sections"].as_array().into_iter().flatten() {
                        if let (Some(path), Some(diff)) =
                            (section["path"].as_str(), section["preview"].as_str())
                        {
                            files.push((
                                path,
                                FileChange::Update {
                                    unified_diff: diff.into(),
                                    move_path: None,
                                },
                            ));
                        } else {
                            missing_diff = true;
                        }
                    }
                }
                Some("mutate.delete") => {
                    // The step carries the removed text, so the card can show what
                    // was there rather than an empty deletion.
                    if let (Some(path), Some(content)) =
                        (report["path"].as_str(), report["content"].as_str())
                    {
                        files.push((
                            path,
                            FileChange::Delete {
                                content: content.into(),
                            },
                        ));
                    } else {
                        missing_diff = true;
                    }
                }
                _ => continue,
            }
            for (file, (path, change)) in files.into_iter().enumerate() {
                let path = self.workspace.join(path);
                if !path.starts_with(&self.workspace)
                    || path
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    missing_diff = true;
                    continue;
                }
                // Separate IDs preserve successive edits to the same file in a batch.
                let mut item = FileChangeItem {
                    id: format!("{call_id}-isohypse-{step}-{file}"),
                    changes: HashMap::from([(path, change)]),
                    status: None,
                    auto_approved: Some(true),
                    stdout: None,
                    stderr: None,
                };
                emitter.emit_file_change(item.clone()).await;
                item.status = Some(PatchApplyStatus::Completed);
                item.stdout = Some("Isohypse mutation and validation completed.".into());
                emitter.emit_file_change(item).await;
            }
        }
        if missing_diff {
            let output = result["output"].as_str().unwrap_or_default();
            result["output"] = json!(format!(
                "{output}\n[Edit completed; structured diff unavailable. Reload the updated Isohypse daemon.]"
            ));
        }
    }
}
