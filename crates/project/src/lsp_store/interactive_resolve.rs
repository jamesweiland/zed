//! Experimental interactive command resolution (`command/resolve`), as implemented by gopls:
//! https://github.com/golang/tools/blob/master/gopls/doc/design/integrating-interactive-refactoring.md

use lsp::ServerCapabilities;
use serde::{Deserialize, Serialize};

/// See https://github.com/golang/tools/blob/master/gopls/doc/design/integrating-interactive-refactoring.md
pub fn supports_command_resolve(capabilities: &ServerCapabilities) -> bool {
    capabilities
        .experimental
        .as_ref()
        .and_then(|experimental| experimental.get("interactiveResolveProvider"))
        .and_then(|provider| provider.get("kinds"))
        .and_then(|kinds| kinds.as_array())
        .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("command")))
}

/// Resolves `ExecuteCommandParams` interactively, looping until the server returns no `formFields`.
pub enum CommandResolve {}

impl lsp::request::Request for CommandResolve {
    type Params = CommandResolveParams;
    type Result = CommandResolveParams;
    const METHOD: &'static str = "command/resolve";
}

/// Like `lsp::ExecuteCommandParams`, but for a command that was resolved interactively.
/// We can't use `lsp::ExecuteCommandParams directly because it doesn't carry `formAnswers`.
pub enum ExecuteResolvedCommand {}

impl lsp::request::Request for ExecuteResolvedCommand {
    type Params = CommandResolveParams;
    type Result = Option<serde_json::Value>;
    const METHOD: &'static str = "workspace/executeCommand";
}

/// Queries the options of a [`FormFieldType::LazyEnum`] field.
pub enum InteractiveListEnum {}

impl lsp::request::Request for InteractiveListEnum {
    type Params = InteractiveListEnumParams;
    type Result = Vec<FormEnumEntry>;
    const METHOD: &'static str = "interactive/listEnum";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FormFieldType {
    String,
    Bool,
    Number,
    File(FormFieldTypeFile),
    Enum(FormFieldTypeEnum),
    LazyEnum(FormFieldTypeLazyEnum),
    List(FormFieldTypeList),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormEnumEntry {
    /// The identifier sent back in `formAnswers` when the user selects this option.
    pub value: String,
    /// The human-readable label presented to the user.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormFieldTypeEnum {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub entries: Vec<FormEnumEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormFieldTypeLazyEnum {
    /// Identifies the data source on the server, e.g. "workspace/symbol".
    pub source: String,
    /// Opaque settings echoed back in [`InteractiveListEnumParams`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Bit set of allowed file existence states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileExistence(pub u32);

impl FileExistence {
    pub const NEW: Self = Self(1 << 0);
    pub const EXISTING: Self = Self(1 << 1);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Bit set of allowed file system resource types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileType(pub u32);

impl FileType {
    pub const REGULAR: Self = Self(1 << 0);
    pub const DIRECTORY: Self = Self(1 << 1);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// An input for a file or directory URI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormFieldTypeFile {
    pub existence: FileExistence,
    #[serde(rename = "type")]
    pub file_type: FileType,
    /// Allowed extensions without the leading dot; empty means any.
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub filters: Vec<String>,
}

/// A homogenous list of items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormFieldTypeList {
    pub element_type: Box<FormFieldType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormField {
    pub id: String,
    pub description: String,
    #[serde(rename = "type")]
    pub field_type: FormFieldType,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// Validation message for the previous answer to this field. If None, then the previous message was valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormAnswer {
    pub id: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveParams {
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub form_fields: Vec<FormField>,
    #[serde(
        default,
        deserialize_with = "null_as_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub form_answers: Vec<FormAnswer>,
}

/// `ExecuteCommandParams` extended with [`InteractiveParams`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandResolveParams {
    #[serde(flatten)]
    pub execute_command: lsp::ExecuteCommandParams,
    #[serde(flatten)]
    pub interactive: InteractiveParams,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveListEnumParams {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// An empty query requests a default set of options.
    pub query: String,
}

/// Deserializes an explicit `null` as the type's default, since Go servers like gopls encode empty slices as `null`.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_null_lists_deserialize_as_empty() {
        let file: FormFieldType = serde_json::from_value(json!({
            "kind": "file",
            "existence": 1,
            "type": 1,
            "filters": null
        }))
        .unwrap();
        assert_eq!(
            file,
            FormFieldType::File(FormFieldTypeFile {
                existence: FileExistence::NEW,
                file_type: FileType::REGULAR,
                filters: Vec::new(),
            })
        );

        let enum_type: FormFieldType =
            serde_json::from_value(json!({ "kind": "enum", "entries": null })).unwrap();
        assert_eq!(
            enum_type,
            FormFieldType::Enum(FormFieldTypeEnum {
                name: None,
                entries: Vec::new(),
            })
        );

        let params: CommandResolveParams = serde_json::from_value(json!({
            "command": "the/command",
            "formFields": null,
            "formAnswers": null
        }))
        .unwrap();
        assert_eq!(params.interactive, InteractiveParams::default());
    }

    #[test]
    fn test_supports_command_resolve() {
        let supports = |experimental: serde_json::Value| {
            supports_command_resolve(&ServerCapabilities {
                experimental: Some(experimental),
                ..Default::default()
            })
        };

        assert!(supports(json!({
            "interactiveResolveProvider": {
                "kinds": [
                    "command"
                ]
            }
        })));
        assert!(!supports(json!({})));
        assert!(!supports(json!({
            "interactiveResolveProvider": {
                "kinds": [
                    "not command"
                ]
            }
        })));
        assert!(supports(json!({
            "interactiveResolveProvider": {
                "kinds": [
                    "not command",
                    "command"
                ]
            }
        })));

        assert!(!supports_command_resolve(&ServerCapabilities::default()));
    }

    #[test]
    fn test_command_resolve_params_round_trip() {
        let response = json!({
            "command": "gopls.modify_tags",
            "arguments": [{ "Modification": "add" }],
            "formFields": [
                {
                    "id": "tags",
                    "description": "comma-separated list of tags to add",
                    "type": { "kind": "string" },
                    "required": true,
                    "default": "json",
                    "error": "cannot contain spaces"
                },
                {
                    "id": "transform",
                    "description": "transform rule for added tags",
                    "type": {
                        "kind": "enum",
                        "entries": [{ "value": "camelcase", "description": "camelCase" }]
                    },
                    "required": true
                },
                {
                    "id": "file",
                    "description": "destination",
                    "type": { "kind": "file", "existence": 3, "type": 1, "filters": ["go"] },
                    "required": false
                },
                {
                    "id": "symbols",
                    "description": "symbols to move",
                    "type": {
                        "kind": "list",
                        "elementType": { "kind": "lazyEnum", "source": "workspace/symbol" }
                    },
                    "required": false
                }
            ],
            "formAnswers": [{ "id": "tags", "value": "json,fo o" }]
        });

        let params: CommandResolveParams = serde_json::from_value(response.clone()).unwrap();
        let field_types: Vec<_> = params
            .interactive
            .form_fields
            .iter()
            .map(|field| field.field_type.clone())
            .collect();
        assert_eq!(
            field_types,
            vec![
                FormFieldType::String,
                FormFieldType::Enum(FormFieldTypeEnum {
                    name: None,
                    entries: vec![FormEnumEntry {
                        value: "camelcase".into(),
                        description: "camelCase".into(),
                    }],
                }),
                FormFieldType::File(FormFieldTypeFile {
                    existence: FileExistence(3),
                    file_type: FileType::REGULAR,
                    filters: vec!["go".into()],
                }),
                FormFieldType::List(FormFieldTypeList {
                    element_type: Box::new(FormFieldType::LazyEnum(FormFieldTypeLazyEnum {
                        source: "workspace/symbol".into(),
                        config: None,
                    })),
                }),
            ]
        );
        assert!(FileExistence(3).contains(FileExistence::NEW));
        assert_eq!(serde_json::to_value(&params).unwrap(), response);

        let list_enum_response = json!({ "kind": "enum", "entries": [] });
        let entries: FormFieldTypeEnum = serde_json::from_value(list_enum_response).unwrap();
        assert!(entries.entries.is_empty());

        let unknown_kind = json!({ "kind": "color" });
        assert!(serde_json::from_value::<FormFieldType>(unknown_kind).is_err());
    }
}
