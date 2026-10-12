use std::{path::Path, str::FromStr as _, sync::Arc};

use anyhow::Result;
use collections::HashMap;
use fs::Fs;
use futures::{FutureExt as _, channel::oneshot, future::LocalBoxFuture};
use gpui::{
    AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task,
    WeakEntity,
};
use language::Buffer;
use lsp::LanguageServerId;
use picker::{Picker, PickerDelegate};
use project::{
    Candidates, CodeAction, LspStore,
    lsp_store::interactive_resolve::{
        CommandResolveParams, FileExistence, FileType, FormAnswer, FormEnumEntry, FormFieldType,
        FormFieldTypeFile, InteractiveListEnumParams, InteractiveParams, supports_command_resolve,
    },
};
use ui::{ListItem, ListItemSpacing, prelude::*};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

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

            let prompter = FieldPrompter {
                workspace: workspace.clone(),
                lsp_store: lsp_store.clone(),
                server_id,
                cx: cx.clone(),
            };
            let mut answers = HashMap::default();
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
                let Some(form_answers) =
                    collect_form_answers(&response.interactive, &mut answers, &prompter).await
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

/// A prompt's result when the user didn't cancel: `None` if they skipped the field.
type Answer = Option<serde_json::Value>;

/// Prompts for every field that has no accepted answer yet, one field at a time.
/// Returns `None` if the user cancelled.
async fn collect_form_answers(
    interactive: &InteractiveParams,
    answers: &mut HashMap<String, Answer>,
    prompter: &FieldPrompter,
) -> Option<Vec<FormAnswer>> {
    for field in &interactive.form_fields {
        // Answers from earlier rounds stay unless the server rejected them.
        if field.error.is_none() && answers.contains_key(&field.id) {
            continue;
        }
        let initial = answers
            .get(&field.id)
            .cloned()
            .flatten()
            .or_else(|| {
                interactive
                    .form_answers
                    .iter()
                    .find(|answer| answer.id == field.id)
                    .map(|answer| answer.value.clone())
            })
            .or_else(|| field.default.clone());
        let prompt = FieldPrompt {
            description: field.description.clone().into(),
            error: field.error.clone().map(Into::into),
            initial,
            skip_label: (!field.required).then_some("Skip"),
        };
        let answer = prompter.prompt(prompt, field.field_type.clone()).await?;
        answers.insert(field.id.clone(), answer);
    }
    Some(
        interactive
            .form_fields
            .iter()
            .filter_map(|field| {
                let value = answers.get(&field.id)?.clone()?;
                Some(FormAnswer {
                    id: field.id.clone(),
                    value,
                })
            })
            .collect(),
    )
}

#[derive(Clone)]
struct FieldPrompt {
    description: SharedString,
    error: Option<SharedString>,
    initial: Option<serde_json::Value>,
    /// Label of the option that leaves the field unanswered; `None` when an answer is required.
    skip_label: Option<&'static str>,
}

#[derive(Clone)]
struct FieldPrompter {
    workspace: WeakEntity<Workspace>,
    lsp_store: Entity<LspStore>,
    server_id: LanguageServerId,
    cx: AsyncWindowContext,
}

impl FieldPrompter {
    fn prompt(
        &self,
        prompt: FieldPrompt,
        field_type: FormFieldType,
    ) -> LocalBoxFuture<'static, Option<Answer>> {
        let prompter = self.clone();
        async move {
            match field_type {
                FormFieldType::String => prompter.prompt_text(prompt, TextKind::String).await,
                FormFieldType::Number => prompter.prompt_text(prompt, TextKind::Number).await,
                FormFieldType::Bool => {
                    let choices = vec![
                        Choice::new("Yes", serde_json::Value::Bool(true)),
                        Choice::new("No", serde_json::Value::Bool(false)),
                    ];
                    prompter
                        .prompt_choice(prompt, ChoiceSource::Static(choices))
                        .await
                }
                FormFieldType::Enum(enum_type) => {
                    let choices = enum_type.entries.into_iter().map(Choice::from).collect();
                    prompter
                        .prompt_choice(prompt, ChoiceSource::Static(choices))
                        .await
                }
                FormFieldType::LazyEnum(lazy_enum) => {
                    let source = ChoiceSource::Lazy {
                        lsp_store: prompter.lsp_store.clone(),
                        server_id: prompter.server_id,
                        source: lazy_enum.source,
                        config: lazy_enum.config,
                    };
                    prompter.prompt_choice(prompt, source).await
                }
                FormFieldType::File(file) => prompter.prompt_file(prompt, file, false).await,
                FormFieldType::List(list) => prompter.prompt_list(prompt, *list.element_type).await,
            }
        }
        .boxed_local()
    }

    async fn prompt_new_path(&self, prompt: FieldPrompt) -> Option<Answer> {
        let cx = self.cx.clone();
        let (root, fs) = self
            .workspace
            .read_with(&cx, |workspace, cx| {
                let project = workspace.project().read(cx);
                let root = project
                    .visible_worktrees(cx)
                    .find_map(|worktree| worktree.read(cx).root_dir())?;
                Some((root, project.fs().clone()))
            })
            .ok()??;
        self.prompt_text(prompt, TextKind::NewPath { root, fs })
            .await
    }

    async fn prompt_text(&self, prompt: FieldPrompt, kind: TextKind) -> Option<Answer> {
        let (answer_tx, answer_rx) = oneshot::channel();
        let mut cx = self.cx.clone();
        self.workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    TextPrompt::new(prompt, kind, answer_tx, window, cx)
                });
            })
            .ok()?;
        answer_rx.await.ok()
    }

    async fn prompt_choice(&self, prompt: FieldPrompt, source: ChoiceSource) -> Option<Answer> {
        let (answer_tx, answer_rx) = oneshot::channel();
        let mut cx = self.cx.clone();
        self.workspace
            .update_in(&mut cx, |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    let delegate = ChoiceDelegate {
                        prompt,
                        source,
                        matches: Vec::new(),
                        selected_index: 0,
                        answer_tx: Some(answer_tx),
                    };
                    Picker::uniform_list(delegate, window, cx).reopenable(false, cx)
                });
            })
            .ok()?;
        answer_rx.await.ok()
    }

    async fn prompt_file(
        &self,
        prompt: FieldPrompt,
        file: FormFieldTypeFile,
        multiple: bool,
    ) -> Option<Answer> {
        if !file.existence.contains(FileExistence::EXISTING) {
            return self.prompt_new_path(prompt).await;
        }
        self.prompt_existing_paths(prompt, file.file_type, multiple)
            .await
    }

    async fn prompt_existing_paths(
        &self,
        prompt: FieldPrompt,
        file_type: FileType,
        multiple: bool,
    ) -> Option<Answer> {
        struct FilePromptToast;
        let toast_id = NotificationId::unique::<FilePromptToast>();
        let mut cx = self.cx.clone();
        let paths = self
            .workspace
            .update_in(&mut cx, |workspace, window, cx| {
                let mut message = prompt.description.to_string();
                if let Some(error) = &prompt.error {
                    message.push_str(&format!(" ({error})"));
                }
                workspace.show_toast(Toast::new(toast_id.clone(), message), cx);

                let directories = file_type.contains(FileType::DIRECTORY);
                let candidates = if !directories {
                    Candidates::Files
                } else if file_type.contains(FileType::REGULAR) {
                    Candidates::Entries
                } else {
                    Candidates::Directories
                };
                workspace.prompt_for_project_path(candidates, window, cx)
            })
            .ok()?;
        let paths = paths.await.ok().flatten().unwrap_or_default();
        self.workspace
            .update(&mut cx, |workspace, cx| {
                workspace.dismiss_toast(&toast_id, cx)
            })
            .log_err();

        let mut uris = paths.iter().filter_map(|path| {
            let uri = lsp::Uri::from_file_path(path).ok()?;
            Some(serde_json::Value::String(uri.to_string()))
        });
        let answer = if multiple {
            let uris = uris.collect::<Vec<_>>();
            (!uris.is_empty()).then(|| serde_json::Value::Array(uris))
        } else {
            uris.next()
        };
        match answer {
            Some(answer) => Some(Some(answer)),
            None if prompt.skip_label.is_some() => Some(None),
            None => None,
        }
    }

    async fn prompt_list(
        &self,
        prompt: FieldPrompt,
        element_type: FormFieldType,
    ) -> Option<Answer> {
        if let FormFieldType::File(file) = &element_type
            && file.existence.contains(FileExistence::EXISTING)
        {
            return self
                .prompt_existing_paths(prompt, file.file_type, true)
                .await;
        }
        let mut items = Vec::new();
        loop {
            let item_prompt = FieldPrompt {
                description: format!("{} (item {})", prompt.description, items.len() + 1).into(),
                error: if items.is_empty() {
                    prompt.error.clone()
                } else {
                    None
                },
                initial: None,
                skip_label: (!items.is_empty() || prompt.skip_label.is_some()).then_some("Done"),
            };
            match self.prompt(item_prompt, element_type.clone()).await? {
                Some(item) => items.push(item),
                None => break,
            }
        }
        if items.is_empty() {
            Some(None)
        } else {
            Some(Some(serde_json::Value::Array(items)))
        }
    }
}

enum TextKind {
    String,
    Number,
    NewPath { root: Arc<Path>, fs: Arc<dyn Fs> },
}

pub(crate) struct TextPrompt {
    input: Entity<InputField>,
    kind: TextKind,
    can_skip: bool,
    answer_tx: Option<oneshot::Sender<Answer>>,
}

impl TextPrompt {
    fn new(
        prompt: FieldPrompt,
        kind: TextKind,
        answer_tx: oneshot::Sender<Answer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut placeholder = match prompt.skip_label {
            Some(skip_label) => format!("Leave empty to {}", skip_label.to_lowercase()),
            None => String::new(),
        };
        if let TextKind::NewPath { root, .. } = &kind {
            let hint = format!("Enter a path relative to {}", root.display());
            placeholder = if placeholder.is_empty() {
                hint
            } else {
                format!("{hint}. {placeholder}")
            };
        }
        let input = cx.new(|cx| {
            let mut input = InputField::new(window, cx, &placeholder).label(prompt.description);
            input.set_error(prompt.error, cx);

            if let Some(initial) = prompt.initial {
                let text = match (initial, &kind) {
                    (serde_json::Value::String(text), TextKind::NewPath { root, .. }) => {
                        relative_path_for_uri(&text, root).unwrap_or(text)
                    }
                    (serde_json::Value::String(text), _) => text,
                    (other, _) => other.to_string(),
                };
                input.set_text(&text, window, cx);
            }
            input
        });
        Self {
            input,
            kind,
            can_skip: prompt.skip_label.is_some(),
            answer_tx: Some(answer_tx),
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text(cx);
        if text.is_empty() && self.can_skip {
            self.finish(None, cx);
            return;
        }
        match &self.kind {
            TextKind::String => self.finish(Some(serde_json::Value::String(text)), cx),
            TextKind::Number => match serde_json::from_str::<serde_json::Number>(text.trim()) {
                Ok(number) => self.finish(Some(serde_json::Value::Number(number)), cx),
                Err(_) => self.set_error("Enter a number", cx),
            },
            TextKind::NewPath { root, fs } => {
                let relative_path = text.trim();
                if relative_path.is_empty() {
                    self.set_error("Enter a path", cx);
                    return;
                }
                let path = root.join(relative_path);
                let (Some(parent), Ok(uri)) = (path.parent(), lsp::Uri::from_file_path(&path))
                else {
                    self.set_error("Invalid path", cx);
                    return;
                };
                let parent = parent.to_path_buf();
                let fs = fs.clone();
                cx.spawn(async move |this, cx| {
                    let parent_exists = fs.is_dir(&parent).await;
                    this.update(cx, |this, cx| {
                        if parent_exists {
                            this.finish(Some(serde_json::Value::String(uri.to_string())), cx);
                        } else {
                            let error = format!("Directory {} does not exist", parent.display());
                            this.set_error(error, cx);
                        }
                    })
                    .log_err();
                })
                .detach();
            }
        }
    }

    fn set_error(&mut self, error: impl Into<SharedString>, cx: &mut Context<Self>) {
        let error = error.into();
        self.input
            .update(cx, |input, cx| input.set_error(Some(error), cx));
    }

    fn finish(&mut self, answer: Answer, cx: &mut Context<Self>) {
        if let Some(answer_tx) = self.answer_tx.take() {
            answer_tx.send(answer).ok();
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for TextPrompt {}
impl ModalView for TextPrompt {}

impl Focusable for TextPrompt {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for TextPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("InteractiveCommandPrompt")
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .elevation_2(cx)
            .w(rems(34.))
            .p_3()
            .child(self.input.clone())
    }
}

fn relative_path_for_uri(uri: &str, root: &Path) -> Option<String> {
    let path = lsp::Uri::from_str(uri).ok()?.to_file_path().ok()?;
    Some(path.strip_prefix(root).ok()?.to_string_lossy().into_owned())
}

#[derive(Clone)]
struct Choice {
    label: SharedString,
    answer: Answer,
}

impl Choice {
    fn new(label: impl Into<SharedString>, value: serde_json::Value) -> Self {
        Self {
            label: label.into(),
            answer: Some(value),
        }
    }
}

impl From<FormEnumEntry> for Choice {
    fn from(entry: FormEnumEntry) -> Self {
        let label = if entry.description.is_empty() {
            entry.value.clone()
        } else {
            entry.description
        };
        Self::new(label, serde_json::Value::String(entry.value))
    }
}

enum ChoiceSource {
    Static(Vec<Choice>),
    Lazy {
        lsp_store: Entity<LspStore>,
        server_id: LanguageServerId,
        source: String,
        config: Option<serde_json::Value>,
    },
}

pub(crate) struct ChoiceDelegate {
    prompt: FieldPrompt,
    source: ChoiceSource,
    matches: Vec<Choice>,
    selected_index: usize,
    answer_tx: Option<oneshot::Sender<Answer>>,
}

impl ChoiceDelegate {
    fn set_matches(&mut self, mut matches: Vec<Choice>) {
        if let Some(skip_label) = self.prompt.skip_label {
            matches.push(Choice {
                label: skip_label.into(),
                answer: None,
            });
        }
        self.selected_index = matches
            .iter()
            .position(|choice| {
                self.prompt.initial.is_some() && choice.answer == self.prompt.initial
            })
            .unwrap_or(0);
        self.matches = matches;
    }
}

impl PickerDelegate for ChoiceDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "interactive command choice"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Search…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        match &self.source {
            ChoiceSource::Static(choices) => {
                let query = query.to_lowercase();
                let matches = choices
                    .iter()
                    .filter(|choice| choice.label.to_lowercase().contains(&query))
                    .cloned()
                    .collect();
                self.set_matches(matches);
                Task::ready(())
            }
            ChoiceSource::Lazy {
                lsp_store,
                server_id,
                source,
                config,
            } => {
                let params = InteractiveListEnumParams {
                    source: source.clone(),
                    config: config.clone(),
                    query,
                };
                let options = lsp_store.update(cx, |lsp_store, cx| {
                    lsp_store.list_interactive_enum(*server_id, params, cx)
                });
                cx.spawn_in(window, async move |picker, cx| {
                    let entries = options.await.log_err().map(|options| options.entries);
                    picker
                        .update(cx, |picker, cx| {
                            let matches = entries
                                .unwrap_or_default()
                                .into_iter()
                                .map(Choice::from)
                                .collect();
                            picker.delegate.set_matches(matches);
                            cx.notify();
                        })
                        .log_err();
                })
            }
        }
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(choice) = self.matches.get(self.selected_index)
            && let Some(answer_tx) = self.answer_tx.take()
        {
            answer_tx.send(choice.answer.clone()).ok();
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_header(&self, _: &mut Window, _: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        Some(
            v_flex()
                .px_3()
                .pt_2()
                .gap_0p5()
                .child(Label::new(self.prompt.description.clone()))
                .when_some(self.prompt.error.clone(), |this, error| {
                    this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                })
                .into_any_element(),
        )
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let choice = self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(choice.label.clone())),
        )
    }
}
