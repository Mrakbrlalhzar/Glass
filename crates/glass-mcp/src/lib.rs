//! glass-mcp — MCP (Model Context Protocol) server exposing the
//! glass automation API as LLM-callable tools over stdio.
//!
//! Run via `glass mcp`. The server enumerates the
//! [`glass_api::skill_catalog`] at startup, registers each skill as
//! an MCP tool, and dispatches every `tools/call` request through
//! [`dispatch::call`] into the glass-api crate. Tool results are
//! emitted as a single JSON-text content block — the same JSON the
//! CLI prints by default.
//!
//! `rust-mcp-sdk` ships a `#[mcp_tool]` macro that would have us
//! define one Rust struct per verb. With 24 verbs that's a lot of
//! boilerplate plus a dispatch macro. Instead we build a single
//! `ServerHandler` that consults `glass_api::skill_catalog` for
//! the tool list (so the catalog stays the single source of truth)
//! and matches on `params.name` in [`dispatch::call`].

mod dispatch;

use std::sync::Arc;

use async_trait::async_trait;
use rust_mcp_sdk::{
    error::SdkResult,
    mcp_server::{server_runtime, McpServerOptions, ServerHandler},
    schema::{
        schema_utils::CallToolError, CallToolRequestParams, CallToolResult, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, RpcError,
        ServerCapabilities, ServerCapabilitiesTools, TextContent, Tool, ToolInputSchema,
    },
    McpServer, StdioTransport, ToMcpServerHandler, TransportOptions,
};

/// Boot the MCP stdio server. Blocks until the client disconnects
/// or stdin closes. Constructs its own tokio runtime so callers
/// (e.g. `glass mcp` from a sync `main`) don't need one.
pub fn serve_stdio() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async { run().await.map_err(|e| anyhow::anyhow!("{e}")) })
}

async fn run() -> SdkResult<()> {
    let server_info = InitializeResult {
        server_info: Implementation {
            name: "glass".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("Glass — mobile-app interactive disassembler".into()),
            description: Some(
                "MCP server exposing Glass's automation API: bundle inspection, \
                 native + DEX symbol queries, disasm, CFG, xrefs, search, strings, \
                 annotations."
                    .into(),
            ),
            icons: vec![],
            website_url: Some("https://github.com/azw413/Glass".into()),
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools {
                list_changed: None,
            }),
            ..Default::default()
        },
        protocol_version: ProtocolVersion::latest().into(),
        instructions: Some(
            "Glass exposes one tool per CLI verb. Path arguments accept any file Glass \
             can open (APK, AAB, IPA, ELF, Mach-O). Tool results are JSON text — parse \
             the `text` field as JSON."
                .into(),
        ),
        meta: None,
    };

    let transport = StdioTransport::new(TransportOptions::default())?;
    let handler = GlassHandler.to_mcp_server_handler();
    let options = McpServerOptions {
        server_details: server_info,
        transport,
        handler,
        task_store: None,
        client_task_store: None,
        message_observer: None,
    };
    let server = server_runtime::create_server(options);
    server.start().await
}

struct GlassHandler;

#[async_trait]
impl ServerHandler for GlassHandler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        let cat = glass_api::skill_catalog();
        let tools = cat.skills.iter().map(skill_to_tool).collect();
        Ok(ListToolsResult {
            tools,
            meta: None,
            next_cursor: None,
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        let args = params.arguments.unwrap_or_default();
        let args_value = serde_json::Value::Object(args);
        match dispatch::call(&params.name, &args_value) {
            Ok(json) => Ok(CallToolResult {
                content: vec![TextContent::new(json, None, None).into()],
                is_error: None,
                meta: None,
                structured_content: None,
            }),
            Err(DispatchError::UnknownTool(name)) => Err(CallToolError::unknown_tool(name)),
            Err(DispatchError::Other(e)) => Ok(CallToolResult {
                content: vec![TextContent::new(
                    format!("{{\"error\":{{\"message\":{}}}}}", json_escape(&e)),
                    None,
                    None,
                )
                .into()],
                is_error: Some(true),
                meta: None,
                structured_content: None,
            }),
        }
    }
}

fn skill_to_tool(s: &glass_api::Skill) -> Tool {
    let (required, properties) = split_schema(&s.input_schema);
    Tool {
        name: s.name.into(),
        description: Some(format!("{}\n\nExample: {}", s.description, s.example)),
        input_schema: ToolInputSchema::new(required, Some(properties), None),
        output_schema: None,
        title: Some(s.name.into()),
        annotations: None,
        execution: None,
        icons: vec![],
        meta: None,
    }
}

/// Pull `required` + `properties` out of one of our hand-written
/// input schemas. Shape is always
/// `{ type: "object", required: [...], properties: { ... } }`.
fn split_schema(
    schema: &serde_json::Value,
) -> (
    Vec<String>,
    std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
) {
    let required: Vec<String> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let properties = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_object().map(|m| (k.clone(), m.clone())))
                .collect()
        })
        .unwrap_or_default();
    (required, properties)
}

fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

#[derive(Debug)]
pub(crate) enum DispatchError {
    UnknownTool(String),
    Other(String),
}

impl From<anyhow::Error> for DispatchError {
    fn from(e: anyhow::Error) -> Self {
        DispatchError::Other(format!("{e:#}"))
    }
}

impl From<serde_json::Error> for DispatchError {
    fn from(e: serde_json::Error) -> Self {
        DispatchError::Other(e.to_string())
    }
}
