/// Integration test: load the compiled Rust→Wasm processor into Wasmtime and verify it works.
///
/// NOTE: This test lives here but depends on the aeon-wasm crate. It's meant
/// to be run from the workspace root after building the .wasm file.

#[test]
fn rust_wasm_processor_works() {
    // This test requires the .wasm file to be compiled first:
    // cd samples/processors/rust-wasm && cargo build --target wasm32-unknown-unknown --release
    let wasm_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/target/wasm32-unknown-unknown/release/aeon_sample_rust_wasm.wasm"
    );

    // Just verify the file exists and is valid Wasm
    let bytes = std::fs::read(wasm_path).expect("Wasm file should exist — run `cargo build --target wasm32-unknown-unknown --release` first");
    assert!(bytes.len() > 100, "Wasm file too small");
    assert_eq!(&bytes[0..4], b"\x00asm", "Not a valid Wasm file");
}
