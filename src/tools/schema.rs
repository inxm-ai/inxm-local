//! Supported JSON-schema subset.
//!
//! Runtime validation supports `type`, `required`, `properties`, `items`, and
//! `enum`.

use serde_json::Value;

/// Optional semantic annotation used by plan inputs to choose an appropriate
/// editor. The annotation deliberately lives alongside the JSON Schema rather
/// than replacing its primitive type: runtime tool arguments remain ordinary
/// JSON values, and path inputs are still represented as strings.
pub(crate) const INPUT_KIND_ANNOTATION: &str = "x-inxm-input-kind";

const INPUT_KIND_VALUE: &str = "value";
const INPUT_KIND_FILE_PATH: &str = "file_path";
const INPUT_KIND_OUTPUT_FILE_PATH: &str = "output_file_path";
const INPUT_KIND_DIRECTORY_PATH: &str = "directory_path";

fn is_supported_input_kind(value: &str) -> bool {
    matches!(
        value,
        INPUT_KIND_VALUE
            | INPUT_KIND_FILE_PATH
            | INPUT_KIND_OUTPUT_FILE_PATH
            | INPUT_KIND_DIRECTORY_PATH
    )
}

pub(super) fn validate_instance(schema: &Value, value: &Value) -> Result<(), String> {
    validate_at(schema, value, "$")
}

pub(super) fn validate_definition(schema: &Value) -> Result<(), String> {
    validate_definition_at(schema, "$")
}

fn validate_definition_at(schema: &Value, path: &str) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| format!("{path}: schema must be an object"))?;
    if let Some(schema_type) = object.get("type") {
        let schema_type = schema_type
            .as_str()
            .ok_or_else(|| format!("{path}.type must be a string"))?;
        if !matches!(
            schema_type,
            "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
        ) {
            return Err(format!("{path}.type '{schema_type}' is unsupported"));
        }
    }
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| format!("{path}.required must be an array"))?;
        if required.iter().any(|name| !name.is_string()) {
            return Err(format!("{path}.required entries must be strings"));
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{path}.properties must be an object"))?;
        for (name, property) in properties {
            validate_definition_at(property, &format!("{path}.properties.{name}"))?;
        }
    }
    if let Some(items) = object.get("items") {
        validate_definition_at(items, &format!("{path}.items"))?;
    }
    if object.get("enum").is_some_and(|value| !value.is_array()) {
        return Err(format!("{path}.enum must be an array"));
    }
    validate_input_kind_annotation(object, path)?;
    for annotation in ["sensitive", "x-sensitive", "writeOnly"] {
        if object
            .get(annotation)
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(format!("{path}.{annotation} must be a boolean"));
        }
    }
    Ok(())
}

fn validate_input_kind_annotation(
    schema: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), String> {
    let Some(annotation) = schema.get(INPUT_KIND_ANNOTATION) else {
        return Ok(());
    };
    let Some(input_kind) = annotation.as_str() else {
        return Err(format!("{path}.{INPUT_KIND_ANNOTATION} must be a string"));
    };
    if !is_supported_input_kind(input_kind) {
        return Err(format!(
            "{path}.{INPUT_KIND_ANNOTATION} '{input_kind}' is unsupported"
        ));
    }
    if matches!(
        input_kind,
        INPUT_KIND_FILE_PATH | INPUT_KIND_OUTPUT_FILE_PATH | INPUT_KIND_DIRECTORY_PATH
    ) && schema.get("type").and_then(Value::as_str) != Some("string")
    {
        return Err(format!(
            "{path}.{INPUT_KIND_ANNOTATION} '{input_kind}' requires schema type 'string'"
        ));
    }
    Ok(())
}

fn validate_at(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Err(format!("{path}: schema is not an object"));
    };
    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let matches = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => crate::support::is_json_integer(value),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => return Err(format!("{path}: unsupported schema type '{expected}'")),
        };
        if !matches {
            return Err(format!(
                "{path}: expected {expected}, got {}",
                value_kind(value)
            ));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{path}: value is not in enum"));
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    return Err(format!("{path}: missing required property '{name}'"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property_schema) in properties {
                if let Some(property) = object.get(name) {
                    validate_at(property_schema, property, &format!("{path}.{name}"))?;
                }
            }
        }
    }
    if let Some(items_schema) = schema.get("items")
        && let Some(items) = value.as_array()
    {
        for (index, item) in items.iter().enumerate() {
            validate_at(items_schema, item, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
