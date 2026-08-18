//! JSON Schema numeric coercion shared by the tool schema, plan input, and
//! tool-binding validators (`src/tools/schema.rs`, `src/plan/types.rs`,
//! `src/validator/tool_binding.rs`).
//!
//! JSON Schema's `integer` type is defined by the *mathematical value*, not
//! the wire representation: `1.0` and `1` are both integers, `1.5` is not.
//! `serde_json` keeps a value written as `1.0` in its float representation,
//! so `Value::as_i64` / `Value::as_u64` alone reject it. All three validators
//! independently got this wrong the same way — this is the one place that
//! decides it.

/// Whether a JSON value satisfies the `integer` schema type.
pub fn is_json_integer(value: &serde_json::Value) -> bool {
    value.as_i64().is_some()
        || value.as_u64().is_some()
        || value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
}
