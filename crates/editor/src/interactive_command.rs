use anyhow::Result;
use gpui::{Entity, Task, Window};
use language::Buffer;
use project::{
    CodeAction,
    lsp_store::interactive_resolve::{
        CommandResolveParams, FormAnswer, FormField, InteractiveParams, supports_command_resolve,
    },
};
use ui::Context;
use workspace::Workspace;

use crate::Editor;

impl Editor {
    /// Applies a code action whose command the language server resolves interactively via `command/resolve`.
    /// Returns `None` when the code action should be applied the regular way instead.
    pub(crate) fn apply_interactive_code_action(
        &mut self,
        buffer: Entity<Buffer>,
        action: CodeAction,
        title: String,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let project = workspace.read(cx).project().clone();
        if !project.read(cx).is_local() {
            return None;
        }
        let lsp_store = project.read(cx).lsp_store();
        let server_id = action.server_id;
        let supports_resolve = lsp_store
            .read(cx)
            .lsp_server_capabilities
            .get(&server_id)
            .is_some_and(supports_command_resolve);
        if !supports_resolve {
            return None;
        }

        let workspace = workspace.downgrade();
        Some(cx.spawn_in(window, async move |editor, cx| {
            // Servers may only fill in the command when the code action is resolved.
            let action = lsp_store
                .update(cx, |lsp_store, cx| {
                    lsp_store.resolve_code_action(&buffer, action, cx)
                })
                .await?;
            let has_edit = action
                .lsp_action
                .edit()
                .is_some_and(|edit| edit.changes.is_some() || edit.document_changes.is_some());
            let command = action.lsp_action.command().cloned();
            let Some(command) = command.filter(|_| !has_edit) else {
                let transaction = lsp_store
                    .update(cx, |lsp_store, cx| {
                        lsp_store.apply_code_action(buffer, action, true, cx)
                    })
                    .await?;
                return Editor::open_project_transaction(
                    &editor,
                    workspace,
                    transaction,
                    title,
                    cx,
                )
                .await;
            };

            let mut params = CommandResolveParams {
                execute_command: lsp::ExecuteCommandParams {
                    command: command.command,
                    arguments: command.arguments.unwrap_or_default(),
                    ..Default::default()
                },
                ..Default::default()
            };
            loop {
                let response = lsp_store
                    .update(cx, |lsp_store, cx| {
                        lsp_store.resolve_lsp_command(server_id, params, cx)
                    })
                    .await?;
                if response.interactive.form_fields.is_empty() {
                    params = response;
                    break;
                }
                // shortcut: answers come from field defaults until the form UI exists, replace with the form modal.
                let Some(form_answers) = default_form_answers(&response.interactive.form_fields)
                else {
                    return Ok(());
                };
                params = CommandResolveParams {
                    interactive: InteractiveParams {
                        form_fields: Vec::new(),
                        form_answers,
                    },
                    ..response
                };
            }

            let transaction = lsp_store
                .update(cx, |lsp_store, cx| {
                    lsp_store.execute_resolved_lsp_command(server_id, params, cx)
                })
                .await?;
            Editor::open_project_transaction(&editor, workspace, transaction, title, cx).await
        }))
    }
}

/// Answers every field with its default, or returns `None` to cancel when that isn't possible.
fn default_form_answers(form_fields: &[FormField]) -> Option<Vec<FormAnswer>> {
    // The server rejected the defaults; sending them again would loop forever.
    if form_fields.iter().any(|field| field.error.is_some()) {
        return None;
    }
    let mut answers = Vec::new();
    for field in form_fields {
        match &field.default {
            Some(value) => answers.push(FormAnswer {
                id: field.id.clone(),
                value: value.clone(),
            }),
            None if field.required => return None,
            None => {}
        }
    }
    Some(answers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use project::lsp_store::interactive_resolve::FormFieldType;
    use serde_json::json;

    fn field(id: &str, required: bool, default: Option<serde_json::Value>) -> FormField {
        FormField {
            id: id.to_string(),
            description: String::new(),
            field_type: FormFieldType::String,
            required,
            default,
            error: None,
        }
    }

    #[test]
    fn test_default_form_answers() {
        assert_eq!(
            default_form_answers(&[
                field("with_default", true, Some(json!("value"))),
                field("optional", false, None),
            ]),
            Some(vec![FormAnswer {
                id: "with_default".to_string(),
                value: json!("value"),
            }])
        );
        assert_eq!(default_form_answers(&[field("required", true, None)]), None);

        let mut rejected = field("rejected", true, Some(json!("value")));
        rejected.error = Some("invalid".to_string());
        assert_eq!(default_form_answers(&[rejected]), None);
    }
}
