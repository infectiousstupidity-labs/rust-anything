//! rust-anything: plan, bridge, verify and ship incremental migrations to Rust.
//!
//! The steps (`analyze`, `bridge`, `harness`, `shadow`, `gate`) are language-neutral in shape;
//! each source language is a plugin module. TypeScript/JavaScript (`ts`) is the first.

pub mod analyze;
pub mod app;
pub mod bridge;
pub mod codegen;
pub mod codegen_batch;
pub mod codegen_batch_native;
pub mod codegen_c;
pub mod codegen_ffi;
pub mod codegen_go;
pub mod codegen_java;
pub mod codegen_py;
pub mod codegen_ruby;
pub mod codegen_tswire;
pub mod error_paths;
pub mod from_tests;
pub mod front_door;
pub mod front_door_http;
pub mod gate;
pub mod harness;
pub mod harness_c;
pub mod harness_go;
pub mod harness_java;
pub mod harness_py;
pub mod harness_ruby;
pub mod http;
pub mod isolation;
#[cfg(test)]
mod js_number;
pub mod lang;
pub mod library;
pub mod lift;
pub mod mcp;
pub mod mcp_policy;
pub mod profile;
pub mod recording;
pub mod resource_limits;
pub mod report;
pub mod risks;
pub mod setup;
pub mod shadow;
pub mod tolerance;
pub mod ts;
pub mod walk;
pub mod whole_app;
pub mod wire;
