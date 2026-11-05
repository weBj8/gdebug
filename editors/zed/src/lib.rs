//! Zed debugger extension: registers the `GDebug` adapter, which runs `gdebug dap`.
use zed_extension_api::{
    self as zed, DebugAdapterBinary, DebugTaskDefinition, StartDebuggingRequestArguments,
    StartDebuggingRequestArgumentsRequest, Worktree,
};

struct GDebug;

impl zed::Extension for GDebug {
    fn new() -> Self {
        GDebug
    }

    fn get_dap_binary(
        &mut self,
        _adapter_name: String,
        config: DebugTaskDefinition,
        user_path: Option<String>,
        worktree: &Worktree,
    ) -> Result<DebugAdapterBinary, String> {
        // The target endpoint speaks gdebug/1, not DAP: Zed's tcp_connection does not apply.
        if config.tcp_connection.is_some() {
            return Err("GDebug: remove tcp_connection; use \"endpoint\" to override the target address (e.g. an SSH local forward)".into());
        }
        let command = user_path
            .or_else(|| worktree.which("gdebug"))
            .ok_or("GDebug: `gdebug` not found on PATH; install the gdebug CLI or set the adapter binary path")?;
        Ok(DebugAdapterBinary {
            command: Some(command),
            arguments: vec!["dap".into()],
            envs: vec![],
            cwd: None,
            connection: None,
            request_args: StartDebuggingRequestArguments {
                configuration: config.config,
                request: StartDebuggingRequestArgumentsRequest::Attach,
            },
        })
    }

    fn dap_request_kind(
        &mut self,
        _adapter_name: String,
        config: serde_json::Value,
    ) -> Result<StartDebuggingRequestArgumentsRequest, String> {
        match config.get("request").and_then(|r| r.as_str()) {
            None | Some("attach") => Ok(StartDebuggingRequestArgumentsRequest::Attach),
            Some(r) => Err(format!("GDebug is attach-only (got request {r:?})")),
        }
    }
}

zed::register_extension!(GDebug);
