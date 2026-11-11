//! Generate TypeScript bindings for marshal's myko entities, commands, and
//! queries — the source of truth the `marshal-opencode` plugin consumes so its
//! wire types can't drift from the Rust definitions.
//!
//! Mirrors myko's own `typegen` bin: `generate_item_types` filters the global
//! `inventory` registry by `CARGO_PKG_NAME`, so run under THIS crate
//! (`-p marshal-entities`) it emits only marshal's types.
//!
//! Usage: cargo run -p marshal-entities --features codegen --bin typegen -- <out-dir>

fn main() {
    // Force-link so the `inventory`-registered item/command/query metadata is
    // present (the macros register lazily; nothing here references the types
    // directly otherwise).
    marshal_entities::link();

    let output_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ts/src/generated".to_string());

    // ts-rs reads TS_RS_EXPORT_DIR for individual per-type file output.
    // SAFETY: single-threaded here, before any library init touches env.
    unsafe { std::env::set_var("TS_RS_EXPORT_DIR", &output_dir) };

    myko::codegen::typescript::generate_item_types(&output_dir)
        .expect("failed to generate TypeScript types");
    println!("wrote marshal TS bindings to {output_dir}");
}
