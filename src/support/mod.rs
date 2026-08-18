//! Small cross-cutting helpers shared by otherwise unrelated subsystems.

mod json_numeric;
mod presence;

pub use json_numeric::is_json_integer;
pub use presence::Presence;
