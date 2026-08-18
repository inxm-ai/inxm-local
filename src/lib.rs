//! `inxm-local` library crate.
//!
//! Core subsystems (ported from `inxm-soloplayer`) plus the desktop app layer.
//! The core follows the same thesis: the LLM is the *compiler*, not the
//! runtime. The desktop app is a chat-first surface over that core.

pub mod app;
pub mod compiler;
pub mod error;
pub mod executor;
pub mod hostenv;
pub mod llm;
pub mod plan;
pub mod repair;
pub mod storage;
pub mod support;
pub mod telemetry;
pub mod tools;
pub mod validator;
