//! Tool catalog — types and YAML loading.
//!
//! Tools are defined in `tools.yaml` (by convention) and referenced by name in plans.
//! The catalog is loaded once at startup and cached for the duration of a command.

use crate::error::ToolError;
use crate::tools::schema::validate_definition;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::path::Path;

// ─── Tool kinds and configuration ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Subprocess,
    Http,
    Mcp,
}

/// Configuration for a subprocess-backed tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubprocessConfig {
    /// The executable to run (on PATH or absolute path).
    pub command: String,
    /// Fixed arguments passed to the executable.
    ///
    /// Dynamic tool inputs are exposed through `INXM_ARGS` and
    /// `INXM_ARG_<KEY>` environment variables. Additionally, an input named
    /// `args` whose value is an array is appended to the child command line.
    /// An input named `capture_status` with value `true` returns command status
    /// as tool data instead of treating a non-zero exit as an execution error.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to inject.
    #[serde(default)]
    pub env: IndexMap<String, String>,
    pub working_dir: Option<String>,
}

/// Configuration for an HTTP-backed tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    pub base_url: String,
    /// HTTP method: `GET`, `POST`, `PUT`, `DELETE`, `PATCH`.
    #[serde(default = "default_http_method")]
    pub method: String,
    /// Path template, may include `{arg}` substitutions from the input.
    #[serde(default)]
    pub path_template: String,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    pub timeout_secs: Option<u64>,
}

fn default_http_method() -> String {
    "GET".to_owned()
}

/// Configuration for an MCP tool, independent of its transport.
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// The specific tool name to call on the MCP server.
    pub tool_name: String,
    pub transport: McpTransport,
}

/// A tool discovered from a server's `tools/list`, before it is turned into
/// a catalog entry (or discarded, if the user doesn't import it).
#[derive(Debug, Clone)]
pub struct McpDiscoveredTool {
    /// The tool name as advertised by the server.
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's input, as advertised by the server.
    pub input_schema: serde_json::Value,
}

/// Transport used to reach an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub enum McpTransport {
    /// Spawn the configured command and speak MCP over its standard streams.
    Stdio {
        server_command: String,
        server_args: Vec<String>,
        server_env: IndexMap<String, String>,
    },
    /// Connect to a remote MCP Streamable HTTP endpoint.
    StreamableHttp { endpoint: String, auth: McpAuth },
}

/// Authentication policy for a Streamable HTTP MCP endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpAuth {
    /// Do not attach credentials.
    #[default]
    None,
    /// Use an OAuth 2.0 authorization-code flow with S256 PKCE.
    #[serde(rename = "oauth")]
    OAuth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpConfigWire {
    tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    server_args: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    server_env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "is_no_auth")]
    auth: McpAuth,
}

#[derive(Serialize)]
struct StdioMcpConfigWire<'a> {
    server_command: &'a str,
    server_args: &'a [String],
    tool_name: &'a str,
    server_env: &'a IndexMap<String, String>,
}

#[derive(Serialize)]
struct RemoteMcpConfigWire<'a> {
    endpoint: &'a str,
    #[serde(skip_serializing_if = "is_no_auth")]
    auth: McpAuth,
    tool_name: &'a str,
}

fn is_no_auth(auth: &McpAuth) -> bool {
    matches!(auth, McpAuth::None)
}

impl Serialize for McpConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.transport {
            McpTransport::Stdio {
                server_command,
                server_args,
                server_env,
            } => StdioMcpConfigWire {
                server_command,
                server_args,
                tool_name: &self.tool_name,
                server_env,
            }
            .serialize(serializer),
            McpTransport::StreamableHttp { endpoint, auth } => RemoteMcpConfigWire {
                endpoint,
                auth: auth.clone(),
                tool_name: &self.tool_name,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for McpConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = McpConfigWire::deserialize(deserializer)?;
        let transport = match (wire.server_command, wire.endpoint) {
            (Some(server_command), None) if matches!(wire.auth, McpAuth::None) => {
                McpTransport::Stdio {
                    server_command,
                    server_args: wire.server_args,
                    server_env: wire.server_env,
                }
            }
            (Some(_), None) => {
                return Err(de::Error::custom("stdio MCP config cannot contain auth"));
            }
            (None, Some(endpoint)) if wire.server_args.is_empty() && wire.server_env.is_empty() => {
                McpTransport::StreamableHttp {
                    endpoint,
                    auth: wire.auth,
                }
            }
            (Some(_), Some(_)) => {
                return Err(de::Error::custom(
                    "MCP config must contain exactly one of server_command or endpoint",
                ));
            }
            (None, None) => {
                return Err(de::Error::custom(
                    "MCP config must contain exactly one of server_command or endpoint",
                ));
            }
            (None, Some(_)) => {
                return Err(de::Error::custom(
                    "remote MCP config cannot contain server_args or server_env",
                ));
            }
        };
        Ok(Self {
            tool_name: wire.tool_name,
            transport,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolConfig {
    Subprocess(SubprocessConfig),
    Http(HttpConfig),
    Mcp(McpConfig),
}

impl ToolConfig {
    pub fn kind(&self) -> ToolKind {
        match self {
            ToolConfig::Subprocess(_) => ToolKind::Subprocess,
            ToolConfig::Http(_) => ToolKind::Http,
            ToolConfig::Mcp(_) => ToolKind::Mcp,
        }
    }
}

/// A single tool entry in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    pub config: ToolConfig,
    /// JSON Schema describing the tool's input parameters.
    /// Used by the validator to check TOOL_CALL argument types.
    #[serde(default = "empty_schema")]
    pub input_schema: serde_json::Value,
    /// JSON Schema describing the tool's output.
    #[serde(default = "empty_schema")]
    pub output_schema: serde_json::Value,
    /// Whether this tool is on the explicit allowlist.
    /// Plans referencing non-allowlisted tools fail validation.
    /// Omission deliberately fails closed and deserialises as `false`.
    #[serde(default)]
    pub allowlisted: bool,
    pub timeout_secs: Option<u64>,
}

fn empty_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object" })
}

impl ToolEntry {
    /// Return the list of required input field names from the JSON Schema.
    pub fn required_inputs(&self) -> Vec<String> {
        self.input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ─── Tool catalog ─────────────────────────────────────────────────────────────

/// The loaded and validated tool catalog.
#[derive(Debug, Clone, Default)]
pub struct ToolCatalog {
    tools: IndexMap<String, ToolEntry>,
}

impl ToolCatalog {
    pub fn new(tools: Vec<ToolEntry>) -> Self {
        let map = tools.into_iter().map(|t| (t.name.clone(), t)).collect();
        Self { tools: map }
    }

    pub fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.tools.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn all(&self) -> impl Iterator<Item = &ToolEntry> {
        self.tools.values()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Load a catalog from a YAML file.
    pub fn load_from_file(path: &Path) -> Result<Self, ToolError> {
        let raw = std::fs::read_to_string(path)?;
        Self::load_from_yaml(&raw)
    }

    /// Load a catalog from a YAML string.
    pub fn load_from_yaml(yaml: &str) -> Result<Self, ToolError> {
        let catalog_file: CatalogFile = serde_yaml::from_str(yaml)?;
        let mut names = std::collections::HashSet::new();
        for tool in &catalog_file.tools {
            validate_tool_entry(tool)?;
            if !names.insert(tool.name.as_str()) {
                return Err(ToolError::Catalog(format!(
                    "duplicate tool name '{}'",
                    tool.name
                )));
            }
        }
        Ok(Self::new(catalog_file.tools))
    }

    /// Serialise the catalog to the on-disk YAML format.
    pub fn to_yaml(&self) -> Result<String, ToolError> {
        let file = CatalogFile {
            tools: self.tools.values().cloned().collect(),
        };
        Ok(serde_yaml::to_string(&file)?)
    }

    /// Persist the catalog to a YAML file (creating parent directories).
    pub fn save_to_file(&self, path: &Path) -> Result<(), ToolError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_yaml()?)?;
        Ok(())
    }
}

fn validate_tool_entry(tool: &ToolEntry) -> Result<(), ToolError> {
    if tool.name.trim().is_empty() {
        return Err(ToolError::Catalog("tool entry has empty name".to_owned()));
    }
    if tool.timeout_secs == Some(0) {
        return Err(catalog_error(
            tool,
            "timeout_secs must be greater than zero",
        ));
    }
    validate_schema(tool, "input_schema", &tool.input_schema)?;
    validate_schema(tool, "output_schema", &tool.output_schema)?;

    match &tool.config {
        ToolConfig::Subprocess(config) if config.command.trim().is_empty() => {
            Err(catalog_error(tool, "subprocess command is empty"))
        }
        ToolConfig::Http(config) => validate_http_config(tool, config),
        ToolConfig::Mcp(config) if config.tool_name.trim().is_empty() => {
            Err(catalog_error(tool, "MCP tool name is empty"))
        }
        ToolConfig::Mcp(config) => validate_mcp_config(tool, config),
        ToolConfig::Subprocess(_) => Ok(()),
    }
}

fn validate_mcp_config(tool: &ToolEntry, config: &McpConfig) -> Result<(), ToolError> {
    match &config.transport {
        McpTransport::Stdio { server_command, .. } if server_command.trim().is_empty() => {
            Err(catalog_error(tool, "MCP server command is empty"))
        }
        McpTransport::Stdio { .. } => Ok(()),
        McpTransport::StreamableHttp { endpoint, auth } => {
            let url = reqwest::Url::parse(endpoint)
                .map_err(|error| catalog_error(tool, &format!("invalid MCP endpoint: {error}")))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(catalog_error(tool, "MCP endpoint must use http or https"));
            }
            if url.host_str().is_none() {
                return Err(catalog_error(tool, "MCP endpoint must have a host"));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(catalog_error(
                    tool,
                    "MCP endpoint must not contain userinfo",
                ));
            }
            if url.fragment().is_some() {
                return Err(catalog_error(
                    tool,
                    "MCP endpoint must not contain a fragment",
                ));
            }
            if matches!(auth, McpAuth::OAuth { .. })
                && url.scheme() != "https"
                && !is_loopback_host(url.host_str().unwrap_or_default())
            {
                return Err(catalog_error(
                    tool,
                    "OAuth MCP endpoint must use https unless its host is loopback",
                ));
            }
            Ok(())
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn validate_http_config(tool: &ToolEntry, config: &HttpConfig) -> Result<(), ToolError> {
    if config.timeout_secs == Some(0) {
        return Err(catalog_error(
            tool,
            "HTTP timeout_secs must be greater than zero",
        ));
    }
    if !matches!(
        config.method.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
    ) {
        return Err(catalog_error(
            tool,
            &format!("unsupported HTTP method '{}'", config.method),
        ));
    }
    if config.base_url.trim().is_empty() && config.path_template.trim().is_empty() {
        return Err(catalog_error(tool, "HTTP URL is empty"));
    }
    if !config.base_url.trim().is_empty() {
        let url = reqwest::Url::parse(&config.base_url)
            .map_err(|error| catalog_error(tool, &format!("invalid HTTP base_url: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(catalog_error(tool, "HTTP base_url must use http or https"));
        }
    }

    let placeholders = extract_placeholders(&config.path_template)
        .map_err(|message| catalog_error(tool, &message))?;
    let properties = tool
        .input_schema
        .get("properties")
        .and_then(serde_json::Value::as_object);
    let required = tool.required_inputs();
    for placeholder in placeholders {
        if properties.is_none_or(|items| !items.contains_key(&placeholder)) {
            return Err(catalog_error(
                tool,
                &format!("HTTP placeholder '{{{placeholder}}}' is not declared in input_schema"),
            ));
        }
        if !required.iter().any(|name| name == &placeholder) {
            return Err(catalog_error(
                tool,
                &format!("HTTP placeholder '{{{placeholder}}}' must be a required input"),
            ));
        }
    }
    Ok(())
}

fn validate_schema(
    tool: &ToolEntry,
    field: &str,
    schema: &serde_json::Value,
) -> Result<(), ToolError> {
    validate_definition(schema)
        .map_err(|message| catalog_error(tool, &format!("{field}: {message}")))
}

fn extract_placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut placeholders = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        if rest[..open].contains('}') {
            return Err("HTTP path_template has an unmatched closing brace".to_owned());
        }
        let after_open = &rest[open + 1..];
        let close = after_open
            .find('}')
            .ok_or_else(|| "HTTP path_template has an unclosed placeholder".to_owned())?;
        let name = &after_open[..close];
        if name.is_empty() || name.contains('{') {
            return Err("HTTP path_template has a malformed placeholder".to_owned());
        }
        placeholders.push(name.to_owned());
        rest = &after_open[close + 1..];
    }
    if rest.contains('}') {
        return Err("HTTP path_template has an unmatched closing brace".to_owned());
    }
    Ok(placeholders)
}

fn catalog_error(tool: &ToolEntry, message: &str) -> ToolError {
    ToolError::Catalog(format!("tool '{}': {message}", tool.name))
}

/// On-disk format for the YAML catalog file.
#[derive(Debug, Serialize, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    tools: Vec<ToolEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD_STDIO_MCP_YAML: &str = r#"tools:
- name: mcp-echo
  description: Echo through MCP
  config:
    kind: mcp
    server_command: npx
    server_args:
    - -y
    - '@example/mcp'
    tool_name: echo
    server_env:
      REGION: eu
  allowlisted: true
"#;

    const SAMPLE_YAML: &str = r#"
tools:
  - name: echo
    description: Echoes its input to stdout
    config:
      kind: subprocess
      command: echo
      args: []
    input_schema:
      type: object
      properties:
        message:
          type: string
      required: [message]
    output_schema:
      type: object
      properties:
        stdout:
          type: string
    allowlisted: true
"#;

    #[test]
    fn parses_sample_catalog() {
        let catalog = ToolCatalog::load_from_yaml(SAMPLE_YAML).unwrap();
        assert_eq!(catalog.len(), 1);
        let echo = catalog.get("echo").unwrap();
        assert_eq!(echo.name, "echo");
        assert!(matches!(echo.config, ToolConfig::Subprocess(_)));
    }

    #[test]
    fn required_inputs_extracted_correctly() {
        let catalog = ToolCatalog::load_from_yaml(SAMPLE_YAML).unwrap();
        let echo = catalog.get("echo").unwrap();
        assert_eq!(echo.required_inputs(), vec!["message"]);
    }

    #[test]
    fn contains_returns_true_for_known_tool() {
        let catalog = ToolCatalog::load_from_yaml(SAMPLE_YAML).unwrap();
        assert!(catalog.contains("echo"));
        assert!(!catalog.contains("nonexistent"));
    }

    #[test]
    fn omitted_allowlisted_field_fails_closed() {
        let yaml = SAMPLE_YAML.replace("    allowlisted: true\n", "");
        let catalog = ToolCatalog::load_from_yaml(&yaml).unwrap();
        assert!(!catalog.get("echo").unwrap().allowlisted);
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let duplicate = SAMPLE_YAML.replace(
            "tools:\n",
            "tools:\n  - name: echo\n    description: duplicate\n    config:\n      kind: subprocess\n      command: printf\n",
        );
        assert_catalog_error(&duplicate, "duplicate tool name 'echo'");
    }

    #[test]
    fn invalid_subprocess_and_mcp_commands_are_rejected() {
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: subprocess\n    command: ' '\n",
            "subprocess command is empty",
        );
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: mcp\n    server_command: ''\n    tool_name: call\n",
            "MCP server command is empty",
        );
    }

    #[test]
    fn old_stdio_mcp_yaml_round_trips_in_the_same_flat_shape() {
        let catalog = ToolCatalog::load_from_yaml(OLD_STDIO_MCP_YAML).unwrap();
        let entry = catalog.get("mcp-echo").unwrap();
        let ToolConfig::Mcp(config) = &entry.config else {
            panic!("expected MCP config")
        };
        assert!(matches!(
            &config.transport,
            McpTransport::Stdio { server_command, server_args, server_env }
                if server_command == "npx"
                    && server_args == &["-y", "@example/mcp"]
                    && server_env.get("REGION").is_some_and(|value| value == "eu")
        ));

        let serialized = catalog.to_yaml().unwrap();
        assert!(serialized.contains("server_command: npx"));
        assert!(serialized.contains("server_args:"));
        assert!(serialized.contains("server_env:"));
        assert!(!serialized.contains("transport:"));
        assert!(!serialized.contains("endpoint:"));
        let reparsed = ToolCatalog::load_from_yaml(&serialized).unwrap();
        assert_eq!(reparsed.len(), 1);
    }

    #[test]
    fn remote_mcp_config_round_trips_and_defaults_to_no_auth() {
        let yaml = "tools:\n- name: remote\n  description: remote\n  config:\n    kind: mcp\n    endpoint: https://mcp.example.com/rpc\n    tool_name: search\n";
        let catalog = ToolCatalog::load_from_yaml(yaml).unwrap();
        let ToolConfig::Mcp(config) = &catalog.get("remote").unwrap().config else {
            panic!("expected MCP config")
        };
        assert_eq!(
            config.transport,
            McpTransport::StreamableHttp {
                endpoint: "https://mcp.example.com/rpc".to_owned(),
                auth: McpAuth::None,
            }
        );
        let serialized = catalog.to_yaml().unwrap();
        assert!(serialized.contains("endpoint: https://mcp.example.com/rpc"));
        assert!(!serialized.contains("auth:"));
    }

    #[test]
    fn invalid_remote_mcp_configs_are_rejected() {
        for (fields, expected) in [
            (
                "    server_command: node\n    endpoint: https://example.com/mcp\n",
                "exactly one",
            ),
            ("", "exactly one"),
            ("    endpoint: ftp://example.com/mcp\n", "http or https"),
            (
                "    endpoint: https://user:pass@example.com/mcp\n",
                "must not contain userinfo",
            ),
            (
                "    endpoint: 'https://example.com/mcp#secret'\n",
                "must not contain a fragment",
            ),
            (
                "    endpoint: http://example.com/mcp\n    auth:\n      mode: oauth\n",
                "must use https",
            ),
        ] {
            let yaml = format!(
                "tools:\n- name: remote\n  description: remote\n  config:\n    kind: mcp\n{fields}    tool_name: search\n"
            );
            assert_catalog_error(&yaml, expected);
        }
        for endpoint in [
            "http://localhost:9999/mcp",
            "http://127.0.0.1:9999/mcp",
            "http://[::1]:9999/mcp",
        ] {
            let yaml = format!(
                "tools:\n- name: remote\n  description: remote\n  config:\n    kind: mcp\n    endpoint: {endpoint}\n    auth:\n      mode: oauth\n    tool_name: search\n"
            );
            ToolCatalog::load_from_yaml(&yaml).unwrap();
        }
    }

    #[test]
    fn mcp_config_rejects_unknown_and_token_like_fields() {
        for unexpected in [
            "    access_token: secret\n",
            "    headers:\n      Authorization: Bearer secret\n",
            "    auth:\n      mode: oauth\n      client_secret: secret\n",
        ] {
            let yaml = format!(
                "tools:\n- name: remote\n  description: remote\n  config:\n    kind: mcp\n    endpoint: https://example.com/mcp\n    tool_name: search\n{unexpected}"
            );
            assert_catalog_error(&yaml, "unknown field");
        }
    }

    #[test]
    fn invalid_timeouts_are_rejected() {
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  timeout_secs: 0\n  config:\n    kind: subprocess\n    command: echo\n",
            "timeout_secs must be greater than zero",
        );
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: http\n    base_url: https://example.com\n    timeout_secs: 0\n",
            "HTTP timeout_secs must be greater than zero",
        );
    }

    #[test]
    fn invalid_http_urls_methods_and_placeholders_are_rejected() {
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: http\n    base_url: ''\n",
            "HTTP URL is empty",
        );
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: http\n    base_url: ftp://example.com\n",
            "must use http or https",
        );
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: http\n    base_url: https://example.com\n    method: BREW\n",
            "unsupported HTTP method",
        );
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: http\n    base_url: https://example.com\n    path_template: /users/{id}\n  input_schema:\n    type: object\n",
            "placeholder '{id}' is not declared",
        );
    }

    #[test]
    fn malformed_schemas_are_rejected() {
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: subprocess\n    command: echo\n  input_schema: []\n",
            "input_schema: $: schema must be an object",
        );
        assert_catalog_error(
            "tools:\n- name: bad\n  description: bad\n  config:\n    kind: subprocess\n    command: echo\n  output_schema:\n    type: 42\n",
            "output_schema: $.type must be a string",
        );
    }

    #[test]
    fn input_kind_annotations_are_preserved_for_string_properties() {
        let yaml = SAMPLE_YAML.replace(
            "          type: string\n",
            "          type: string\n          x-inxm-input-kind: file_path\n",
        );
        let catalog = ToolCatalog::load_from_yaml(&yaml).unwrap();
        assert_eq!(
            catalog.get("echo").unwrap().input_schema["properties"]["message"]["x-inxm-input-kind"],
            "file_path"
        );
    }

    #[test]
    fn input_kind_annotations_accept_all_supported_path_kinds() {
        for input_kind in ["file_path", "output_file_path", "directory_path"] {
            let yaml = SAMPLE_YAML.replace(
                "          type: string\n",
                &format!("          type: string\n          x-inxm-input-kind: {input_kind}\n"),
            );
            let catalog = ToolCatalog::load_from_yaml(&yaml).unwrap();
            assert_eq!(
                catalog.get("echo").unwrap().input_schema["properties"]["message"]["x-inxm-input-kind"],
                input_kind
            );
        }
    }

    #[test]
    fn input_kind_annotations_must_use_supported_values() {
        let yaml = SAMPLE_YAML.replace(
            "          type: string\n",
            "          type: string\n          x-inxm-input-kind: socket_path\n",
        );
        assert_catalog_error(&yaml, "x-inxm-input-kind 'socket_path' is unsupported");
    }

    #[test]
    fn path_input_kind_annotations_require_string_schema_types() {
        let yaml = SAMPLE_YAML.replace(
            "          type: string\n",
            "          type: integer\n          x-inxm-input-kind: directory_path\n",
        );
        assert_catalog_error(
            &yaml,
            "x-inxm-input-kind 'directory_path' requires schema type 'string'",
        );
    }

    #[test]
    fn value_input_kind_annotation_supports_non_string_schema_types() {
        let yaml = SAMPLE_YAML.replace(
            "          type: string\n",
            "          type: integer\n          x-inxm-input-kind: value\n",
        );
        let catalog = ToolCatalog::load_from_yaml(&yaml).unwrap();
        assert_eq!(
            catalog.get("echo").unwrap().input_schema["properties"]["message"]["type"],
            "integer"
        );
    }

    #[test]
    fn input_kind_annotation_must_be_a_string() {
        let yaml = SAMPLE_YAML.replace(
            "          type: string\n",
            "          type: string\n          x-inxm-input-kind: 7\n",
        );
        assert_catalog_error(&yaml, "x-inxm-input-kind must be a string");
    }

    fn assert_catalog_error(yaml: &str, expected: &str) {
        let error = ToolCatalog::load_from_yaml(yaml).unwrap_err().to_string();
        assert!(
            error.contains(expected),
            "expected '{expected}' in catalog error, got: {error}"
        );
    }
}
