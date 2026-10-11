use rmcp::ServerHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router};

use crate::catalog::Ctx;
use crate::client::Client;
use crate::error::ToolError;
use crate::tools::{
    GetAttachmentArgs, GetIssueArgs, ListProjectsArgs, SearchArgs, get_attachment, get_issue,
    list_projects, search,
};
use crate::update::UpdateIssueArgs;

#[derive(Clone)]
pub struct RedmineServer {
    ctx: Ctx,
}

impl RedmineServer {
    pub fn new(client: Client) -> Self {
        Self {
            ctx: Ctx::new(client),
        }
    }
}

#[tool_router]
impl RedmineServer {
    #[tool(
        title = "Get issue",
        description = "Fetch one issue, or up to 20 ids for summary rows. Ids can be 123, #123, an issue URL, or a configured slug such as ticket-123. view=summary is the default and does not fetch journals. view=description adds the body, view=journals adds comments newest first, view=full adds both. A long description is returned in windows of about REDMINE_MAX_DESCRIPTION_CHARS that end on a line break. Pass description_next_offset as description_offset with view=description to read the next window, and pass the updated_on from that hint as expected_updated_on if you edit. Summary includes checklist item ids when that plugin is enabled. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn get_issue(
        &self,
        Parameters(args): Parameters<GetIssueArgs>,
    ) -> Result<String, String> {
        get_issue(&self.ctx, args).await.map_err(text)
    }

    #[tool(
        title = "List projects",
        description = "List projects visible to the API key: id, identifier, name, and updated. Use the id or identifier as project_id later. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn list_projects(
        &self,
        Parameters(args): Parameters<ListProjectsArgs>,
    ) -> Result<String, String> {
        list_projects(&self.ctx.client, args).await.map_err(text)
    }

    #[tool(
        title = "Search",
        description = "Full-text search. An id finder, not a reporting tool: it returns ids and clipped titles. snippet_chars defaults to 0, which is titles only. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn search(&self, Parameters(args): Parameters<SearchArgs>) -> Result<String, String> {
        search(&self.ctx.client, args).await.map_err(text)
    }

    #[tool(
        title = "Get attachment",
        description = "Fetch metadata for an attachment by id. Set fetch=true to download the body, only for text types, and only up to REDMINE_MAX_ATTACHMENT_BYTES. Read-only.",
        annotations(read_only_hint = true)
    )]
    async fn get_attachment(
        &self,
        Parameters(args): Parameters<GetAttachmentArgs>,
    ) -> Result<String, String> {
        get_attachment(&self.ctx.client, args).await.map_err(text)
    }

    #[tool(
        title = "Update issue",
        description = "Post a note, edit the description, and add, check, uncheck, rename, or remove checklist items, in one request so they land in one journal entry. A note is required when the description changes. Description edits are find and replace pairs spliced into the current text, so everything outside an edited span stays byte-identical. Checklist items are addressed by the ids get_issue shows. dry_run previews without writing. Writes require REDMINE_READONLY=false.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    async fn update_issue(
        &self,
        Parameters(args): Parameters<UpdateIssueArgs>,
    ) -> Result<String, String> {
        crate::update::update_issue(&self.ctx.client, args)
            .await
            .map_err(text)
    }
}

#[tool_handler(name = "redmine-dev-mcp")]
impl ServerHandler for RedmineServer {
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        let mut tools = Self::tool_router().list_all();
        tools.extend(crate::filters::filter_tools(self.ctx.client.config()));
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let name = request.name.to_string();
        let args = request.arguments.clone().unwrap_or_default();
        let outcome = match name.as_str() {
            "list_issues" => crate::filters::list_issues(&self.ctx, &args).await,
            "count_issues" => crate::filters::count_issues(&self.ctx, &args).await,
            "facet_issues" => crate::filters::facet_issues(&self.ctx, &args).await,
            "list_meta" => crate::filters::list_meta(&self.ctx, &args).await,
            _ => {
                let context =
                    rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
                return Self::tool_router().call(context).await;
            }
        };
        Ok(match outcome {
            Ok(text) => {
                rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)])
                    .into()
            }
            Err(err) => rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(
                err.to_string(),
            )])
            .into(),
        })
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        if let Some(tool) = crate::filters::filter_tool(self.ctx.client.config(), name) {
            return Some(tool);
        }
        Self::tool_router().get(name).cloned()
    }
}

/// The MCP boundary. Errors become tool results with `isError` set, so the model
/// reads the message instead of the call failing at the protocol level.
fn text(err: ToolError) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_use_the_wire_names() {
        let search = RedmineServer::search_tool_attr();
        let schema = serde_json::Value::Object(search.input_schema.as_ref().clone());
        let text = schema.to_string();
        assert!(text.contains("\"my_projects\""), "{text}");
        assert!(text.contains("\"bookmarks\""), "{text}");
        assert!(text.contains("markdown"), "{text}");
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == "q")),
            "{text}"
        );

        let projects = RedmineServer::list_projects_tool_attr();
        assert_eq!(projects.name.as_ref(), "list_projects");
        let attachment = RedmineServer::get_attachment_tool_attr();
        assert_eq!(attachment.name.as_ref(), "get_attachment");

        let issue = RedmineServer::get_issue_tool_attr();
        assert_eq!(issue.name.as_ref(), "get_issue");
        let schema = serde_json::Value::Object(issue.input_schema.as_ref().clone());
        let text = schema.to_string();
        assert!(text.contains("\"summary\""), "{text}");
        assert!(text.contains("\"journals\""), "{text}");
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == "id")),
            "{text}"
        );

        let update = RedmineServer::update_issue_tool_attr();
        assert_eq!(update.name.as_ref(), "update_issue");
        let annotations = update.annotations.as_ref().unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(true));
        assert_eq!(annotations.idempotent_hint, Some(false));
        let schema = serde_json::Value::Object(update.input_schema.as_ref().clone());
        let text = schema.to_string();
        assert!(text.contains("\"dry_run\""), "{text}");
        assert!(text.contains("\"old_string\""), "{text}");
        assert!(text.contains("\"replace_all\""), "{text}");
        assert_eq!(schema["required"], serde_json::json!(["id"]));
    }
}
