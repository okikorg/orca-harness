use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::{PluginError, PluginWarning, PLUGIN_SCHEMA};

const FIELDS: &[&str] = &[
    "$schema",
    "name",
    "version",
    "description",
    "author",
    "homepage",
    "repository",
    "license",
    "keywords",
    "extensions",
];

pub(super) struct ParsedManifest {
    pub name: String,
    pub version: Option<String>,
    pub extension_names: Vec<String>,
    pub warnings: Vec<PluginWarning>,
}

pub(super) fn parse(bytes: &[u8]) -> Result<ParsedManifest, PluginError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| PluginError::new(format!("invalid plugin.json JSON: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| PluginError::new("plugin.json must be an object"))?;

    require_schema(object)?;
    let name = require_string(object, "name")?.to_owned();
    validate_name(&name)?;
    let version = optional_string(object, "version")?.map(str::to_owned);
    for field in ["description", "homepage", "repository", "license"] {
        optional_string(object, field)?;
    }
    validate_author(object.get("author"))?;
    validate_keywords(object.get("keywords"))?;

    let known: BTreeSet<_> = FIELDS.iter().copied().collect();
    let mut warnings = object
        .keys()
        .filter(|field| !known.contains(field.as_str()))
        .map(|field| {
            PluginWarning::new(
                format!("plugin.json.{field}"),
                "unknown manifest field ignored",
            )
        })
        .collect::<Vec<_>>();
    let extension_names = match object.get("extensions") {
        Some(Value::Object(extensions)) => extensions.keys().cloned().collect(),
        Some(_) => {
            warnings.push(PluginWarning::new(
                "plugin.json.extensions",
                "non-object extensions field ignored",
            ));
            Vec::new()
        }
        None => Vec::new(),
    };

    Ok(ParsedManifest {
        name,
        version,
        extension_names,
        warnings,
    })
}

fn require_schema(object: &Map<String, Value>) -> Result<(), PluginError> {
    let schema = require_string(object, "$schema")?;
    if schema != PLUGIN_SCHEMA {
        return Err(PluginError::new(format!(
            "unsupported plugin.json $schema: {schema}"
        )));
    }
    Ok(())
}

fn require_string<'a>(object: &'a Map<String, Value>, field: &str) -> Result<&'a str, PluginError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::new(format!("plugin.json field {field} must be a string")))
}

fn optional_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>, PluginError> {
    match object.get(field) {
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(PluginError::new(format!(
            "plugin.json field {field} must be a string"
        ))),
        None => Ok(None),
    }
}

fn validate_name(name: &str) -> Result<(), PluginError> {
    let bytes = name.as_bytes();
    let valid = (1..=64).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && !name.contains("--")
        && !name.contains("..");
    if !valid {
        return Err(PluginError::new(
            "plugin.json name must be 1-64 lowercase ASCII letters, digits, hyphens, or periods; begin and end alphanumeric; and contain neither -- nor ..",
        ));
    }
    Ok(())
}

fn validate_author(value: Option<&Value>) -> Result<(), PluginError> {
    let Some(value) = value else {
        return Ok(());
    };
    let author = value
        .as_object()
        .ok_or_else(|| PluginError::new("plugin.json field author must be an object"))?;
    for (field, value) in author {
        if !matches!(field.as_str(), "name" | "email" | "url") || !value.is_string() {
            return Err(PluginError::new(format!(
                "plugin.json author field {field} is not permitted or is not a string"
            )));
        }
    }
    Ok(())
}

fn validate_keywords(value: Option<&Value>) -> Result<(), PluginError> {
    let Some(value) = value else {
        return Ok(());
    };
    if !value
        .as_array()
        .is_some_and(|values| values.iter().all(Value::is_string))
    {
        return Err(PluginError::new(
            "plugin.json field keywords must be an array of strings",
        ));
    }
    Ok(())
}
