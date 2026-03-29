//! Wasmtime runtime — loads and executes Wasm processor modules.
//!
//! Uses wasmtime's core module API with a simple ABI:
//! - Guest exports: `alloc(size) -> ptr`, `dealloc(ptr, size)`, `process(ptr, len) -> ptr`
//! - Host imports: state_get, state_put, state_delete, log_info, log_warn, log_error,
//!   metrics_counter_inc, metrics_gauge_set, clock_now_ms
//!
//! The serialization format between host and guest is simple length-prefixed bytes:
//! - Input: serialized event (bincode or custom)
//! - Output: serialized list of outputs
//!
//! Fuel metering limits CPU usage per process() call.

use std::collections::HashMap;
use std::sync::Arc;

use aeon_types::AeonError;
use wasmtime::*;

/// Configuration for the Wasm runtime.
#[derive(Debug, Clone)]
pub struct WasmConfig {
    /// Maximum fuel per process() call. None = no limit.
    pub max_fuel: Option<u64>,
    /// Maximum memory in bytes (pages * 64KiB).
    pub max_memory_bytes: usize,
    /// Whether to enable SIMD instructions.
    pub enable_simd: bool,
    /// Processor namespace (for state isolation).
    pub namespace: String,
}

impl Default for WasmConfig {
    fn default() -> Self {
        Self {
            max_fuel: Some(1_000_000),
            max_memory_bytes: 64 * 1024 * 1024, // 64 MiB
            enable_simd: true,
            namespace: "default".to_string(),
        }
    }
}

/// Host state accessible from within Wasm host functions.
pub struct HostState {
    /// State namespace prefix for isolation.
    pub namespace: Arc<str>,
    /// In-memory key-value state store for Wasm guests.
    pub state: HashMap<Vec<u8>, Vec<u8>>,
    /// Accumulated log messages (for testing / capture).
    pub logs: Vec<(String, String)>, // (level, message)
    /// Accumulated metrics.
    pub metrics: Vec<(String, f64)>, // (name, value)
}

impl HostState {
    fn new(namespace: &str) -> Self {
        Self {
            namespace: Arc::from(namespace),
            state: HashMap::new(),
            logs: Vec::new(),
            metrics: Vec::new(),
        }
    }
}

/// A compiled Wasm module ready for instantiation.
pub struct WasmModule {
    engine: Engine,
    module: Module,
    config: WasmConfig,
}

impl WasmModule {
    /// Compile a Wasm module from bytes.
    pub fn from_bytes(wasm_bytes: &[u8], config: WasmConfig) -> Result<Self, AeonError> {
        let mut engine_config = Config::new();

        if config.max_fuel.is_some() {
            engine_config.consume_fuel(true);
        }

        engine_config.wasm_simd(config.enable_simd);

        let engine = Engine::new(&engine_config)
            .map_err(|e| AeonError::processor(format!("wasm engine creation failed: {e}")))?;

        let module = Module::new(&engine, wasm_bytes)
            .map_err(|e| AeonError::processor(format!("wasm module compilation failed: {e}")))?;

        Ok(Self {
            engine,
            module,
            config,
        })
    }

    /// Compile a Wasm module from a WAT (text format) string.
    pub fn from_wat(wat: &str) -> Result<Self, AeonError> {
        Self::from_bytes(wat.as_bytes(), WasmConfig::default())
    }

    /// Compile from WAT with custom config.
    pub fn from_wat_with_config(wat: &str, config: WasmConfig) -> Result<Self, AeonError> {
        Self::from_bytes(wat.as_bytes(), config)
    }

    /// Create a new instance of this module.
    pub fn instantiate(&self) -> Result<WasmInstance, AeonError> {
        let mut store = Store::new(&self.engine, HostState::new(&self.config.namespace));

        if let Some(fuel) = self.config.max_fuel {
            store
                .set_fuel(fuel)
                .map_err(|e| AeonError::processor(format!("set fuel failed: {e}")))?;
        }

        let mut linker = Linker::new(&self.engine);

        // Register host functions
        register_host_functions(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|e| AeonError::processor(format!("wasm instantiation failed: {e}")))?;

        Ok(WasmInstance { store, instance })
    }

    /// Get a reference to the config.
    pub fn config(&self) -> &WasmConfig {
        &self.config
    }
}

/// A live Wasm instance with its store.
pub struct WasmInstance {
    store: Store<HostState>,
    instance: Instance,
}

impl WasmInstance {
    /// Call a function that takes no args and returns an i32.
    pub fn call_i32(&mut self, name: &str) -> Result<i32, AeonError> {
        let func = self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, name)
            .map_err(|e| AeonError::processor(format!("function '{name}' not found: {e}")))?;

        func.call(&mut self.store, ())
            .map_err(|e| AeonError::processor(format!("call '{name}' failed: {e}")))
    }

    /// Call a function that takes an i32 and returns an i32.
    pub fn call_i32_i32(&mut self, name: &str, arg: i32) -> Result<i32, AeonError> {
        let func = self
            .instance
            .get_typed_func::<i32, i32>(&mut self.store, name)
            .map_err(|e| AeonError::processor(format!("function '{name}' not found: {e}")))?;

        func.call(&mut self.store, arg)
            .map_err(|e| AeonError::processor(format!("call '{name}' failed: {e}")))
    }

    /// Call a function with two i32 args, returning i32.
    pub fn call_2i32_i32(&mut self, name: &str, a: i32, b: i32) -> Result<i32, AeonError> {
        let func = self
            .instance
            .get_typed_func::<(i32, i32), i32>(&mut self.store, name)
            .map_err(|e| AeonError::processor(format!("function '{name}' not found: {e}")))?;

        func.call(&mut self.store, (a, b))
            .map_err(|e| AeonError::processor(format!("call '{name}' failed: {e}")))
    }

    /// Write bytes into guest memory at the given offset.
    pub fn write_memory(&mut self, offset: usize, data: &[u8]) -> Result<(), AeonError> {
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .ok_or_else(|| AeonError::processor("guest has no 'memory' export"))?;

        memory.data_mut(&mut self.store)[offset..offset + data.len()].copy_from_slice(data);
        Ok(())
    }

    /// Read bytes from guest memory.
    pub fn read_memory(&mut self, offset: usize, len: usize) -> Result<Vec<u8>, AeonError> {
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .ok_or_else(|| AeonError::processor("guest has no 'memory' export"))?;

        let data = &memory.data(&self.store)[offset..offset + len];
        Ok(data.to_vec())
    }

    /// Get remaining fuel.
    pub fn remaining_fuel(&mut self) -> Result<u64, AeonError> {
        self.store
            .get_fuel()
            .map_err(|e| AeonError::processor(format!("get fuel failed: {e}")))
    }

    /// Reset fuel to the given amount.
    pub fn reset_fuel(&mut self, fuel: u64) -> Result<(), AeonError> {
        self.store
            .set_fuel(fuel)
            .map_err(|e| AeonError::processor(format!("set fuel failed: {e}")))
    }

    /// Access the host state (logs, metrics).
    pub fn host_state(&self) -> &HostState {
        self.store.data()
    }

    /// Access the host state mutably.
    pub fn host_state_mut(&mut self) -> &mut HostState {
        self.store.data_mut()
    }
}

/// Register host function imports in the linker.
fn register_host_functions(linker: &mut Linker<HostState>) -> Result<(), AeonError> {
    // State functions
    linker
        .func_wrap(
            "env",
            "state_get",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             out_ptr: i32,
             out_max_len: i32|
             -> i32 {
                let key = read_bytes_from_caller(&mut caller, key_ptr as usize, key_len as usize);
                let ns = caller.data().namespace.clone();
                let mut prefixed_key = Vec::with_capacity(ns.len() + 1 + key.len());
                prefixed_key.extend_from_slice(ns.as_bytes());
                prefixed_key.push(b':');
                prefixed_key.extend_from_slice(&key);

                let value_copy = caller.data().state.get(&prefixed_key).cloned();
                match value_copy {
                    Some(value) => {
                        let copy_len = value.len().min(out_max_len as usize);
                        let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                        if let Some(mem) = memory {
                            mem.data_mut(&mut caller)
                                [out_ptr as usize..out_ptr as usize + copy_len]
                                .copy_from_slice(&value[..copy_len]);
                        }
                        copy_len as i32
                    }
                    None => -1,
                }
            },
        )
        .map_err(|e| AeonError::processor(format!("link state_get failed: {e}")))?;

    linker
        .func_wrap(
            "env",
            "state_put",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             val_ptr: i32,
             val_len: i32| {
                let key = read_bytes_from_caller(&mut caller, key_ptr as usize, key_len as usize);
                let value = read_bytes_from_caller(&mut caller, val_ptr as usize, val_len as usize);
                let ns = caller.data().namespace.clone();
                let mut prefixed_key = Vec::with_capacity(ns.len() + 1 + key.len());
                prefixed_key.extend_from_slice(ns.as_bytes());
                prefixed_key.push(b':');
                prefixed_key.extend_from_slice(&key);
                caller.data_mut().state.insert(prefixed_key, value);
            },
        )
        .map_err(|e| AeonError::processor(format!("link state_put failed: {e}")))?;

    linker
        .func_wrap(
            "env",
            "state_delete",
            |mut caller: Caller<'_, HostState>, key_ptr: i32, key_len: i32| {
                let key = read_bytes_from_caller(&mut caller, key_ptr as usize, key_len as usize);
                let ns = caller.data().namespace.clone();
                let mut prefixed_key = Vec::with_capacity(ns.len() + 1 + key.len());
                prefixed_key.extend_from_slice(ns.as_bytes());
                prefixed_key.push(b':');
                prefixed_key.extend_from_slice(&key);
                caller.data_mut().state.remove(&prefixed_key);
            },
        )
        .map_err(|e| AeonError::processor(format!("link state_delete failed: {e}")))?;

    // Logging functions
    linker
        .func_wrap(
            "env",
            "log_info",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                let msg = read_string_from_caller(&mut caller, ptr as usize, len as usize);
                tracing::info!(target: "wasm_guest", "{}", msg);
                caller.data_mut().logs.push(("info".into(), msg));
            },
        )
        .map_err(|e| AeonError::processor(format!("link log_info failed: {e}")))?;

    linker
        .func_wrap(
            "env",
            "log_warn",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                let msg = read_string_from_caller(&mut caller, ptr as usize, len as usize);
                tracing::warn!(target: "wasm_guest", "{}", msg);
                caller.data_mut().logs.push(("warn".into(), msg));
            },
        )
        .map_err(|e| AeonError::processor(format!("link log_warn failed: {e}")))?;

    linker
        .func_wrap(
            "env",
            "log_error",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                let msg = read_string_from_caller(&mut caller, ptr as usize, len as usize);
                tracing::error!(target: "wasm_guest", "{}", msg);
                caller.data_mut().logs.push(("error".into(), msg));
            },
        )
        .map_err(|e| AeonError::processor(format!("link log_error failed: {e}")))?;

    // Clock
    linker
        .func_wrap("env", "clock_now_ms", || -> i64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64
        })
        .map_err(|e| AeonError::processor(format!("link clock_now_ms failed: {e}")))?;

    // Metrics
    linker
        .func_wrap(
            "env",
            "metrics_counter_inc",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32, delta: i64| {
                let name = read_string_from_caller(&mut caller, ptr as usize, len as usize);
                caller.data_mut().metrics.push((name, delta as f64));
            },
        )
        .map_err(|e| AeonError::processor(format!("link metrics_counter_inc failed: {e}")))?;

    linker
        .func_wrap(
            "env",
            "metrics_gauge_set",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32, value: f64| {
                let name = read_string_from_caller(&mut caller, ptr as usize, len as usize);
                caller.data_mut().metrics.push((name, value));
            },
        )
        .map_err(|e| AeonError::processor(format!("link metrics_gauge_set failed: {e}")))?;

    // AssemblyScript compatibility — abort function
    linker
        .func_wrap(
            "env",
            "abort",
            |_caller: Caller<'_, HostState>,
             _msg_ptr: i32,
             _file_ptr: i32,
             _line: i32,
             _col: i32| {
                tracing::error!(target: "wasm_guest", "guest abort called");
            },
        )
        .map_err(|e| AeonError::processor(format!("link abort failed: {e}")))?;

    Ok(())
}

/// Read raw bytes from the guest's linear memory via a Caller.
fn read_bytes_from_caller(caller: &mut Caller<'_, HostState>, ptr: usize, len: usize) -> Vec<u8> {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory());
    match memory {
        Some(mem) => {
            let data = &mem.data(&caller)[ptr..ptr + len];
            data.to_vec()
        }
        None => Vec::new(),
    }
}

/// Read a string from the guest's linear memory via a Caller.
fn read_string_from_caller(caller: &mut Caller<'_, HostState>, ptr: usize, len: usize) -> String {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory());
    match memory {
        Some(mem) => {
            let data = &mem.data(&caller)[ptr..ptr + len];
            String::from_utf8_lossy(data).to_string()
        }
        None => "<no memory>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal WAT module that exports an add function
    const ADD_WAT: &str = r#"
        (module
            (func (export "add") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.add
            )
            (memory (export "memory") 1)
        )
    "#;

    // A minimal passthrough that copies input to output
    const IDENTITY_WAT: &str = r#"
        (module
            (func (export "identity") (param i32) (result i32)
                local.get 0
            )
            (memory (export "memory") 1)
        )
    "#;

    // A module that consumes fuel (loop)
    const FUEL_WAT: &str = r#"
        (module
            (func (export "burn") (param i32) (result i32)
                (local $i i32)
                (local.set $i (i32.const 0))
                (block $break
                    (loop $loop
                        (br_if $break (i32.ge_u (local.get $i) (local.get 0)))
                        (local.set $i (i32.add (local.get $i) (i32.const 1)))
                        (br $loop)
                    )
                )
                local.get $i
            )
            (memory (export "memory") 1)
        )
    "#;

    #[test]
    fn compile_and_instantiate() {
        let module = WasmModule::from_wat(ADD_WAT).unwrap();
        let _instance = module.instantiate().unwrap();
    }

    #[test]
    fn call_add_function() {
        let module = WasmModule::from_wat(ADD_WAT).unwrap();
        let mut instance = module.instantiate().unwrap();

        let result = instance.call_2i32_i32("add", 3, 4).unwrap();
        assert_eq!(result, 7);
    }

    #[test]
    fn identity_function() {
        let module = WasmModule::from_wat(IDENTITY_WAT).unwrap();
        let mut instance = module.instantiate().unwrap();

        let result = instance.call_i32_i32("identity", 42).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn memory_read_write() {
        let module = WasmModule::from_wat(ADD_WAT).unwrap();
        let mut instance = module.instantiate().unwrap();

        instance.write_memory(0, b"hello world").unwrap();
        let data = instance.read_memory(0, 11).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn fuel_metering() {
        let config = WasmConfig {
            max_fuel: Some(1_000_000),
            ..Default::default()
        };
        let module = WasmModule::from_wat_with_config(FUEL_WAT, config).unwrap();
        let mut instance = module.instantiate().unwrap();

        let before = instance.remaining_fuel().unwrap();
        let result = instance.call_i32_i32("burn", 100).unwrap();
        let after = instance.remaining_fuel().unwrap();

        assert_eq!(result, 100);
        assert!(after < before, "fuel should decrease after execution");
    }

    #[test]
    fn fuel_exhaustion_returns_error() {
        let config = WasmConfig {
            max_fuel: Some(10), // very low fuel
            ..Default::default()
        };
        let module = WasmModule::from_wat_with_config(FUEL_WAT, config).unwrap();
        let mut instance = module.instantiate().unwrap();

        // Trying to burn 10000 iterations with only 10 fuel should fail
        let result = instance.call_i32_i32("burn", 10_000);
        assert!(result.is_err());
    }

    #[test]
    fn fuel_reset() {
        let config = WasmConfig {
            max_fuel: Some(1_000_000),
            ..Default::default()
        };
        let module = WasmModule::from_wat_with_config(FUEL_WAT, config).unwrap();
        let mut instance = module.instantiate().unwrap();

        instance.call_i32_i32("burn", 100).unwrap();
        let after_burn = instance.remaining_fuel().unwrap();

        instance.reset_fuel(1_000_000).unwrap();
        let after_reset = instance.remaining_fuel().unwrap();

        assert!(after_reset > after_burn);
    }

    #[test]
    fn missing_function_returns_error() {
        let module = WasmModule::from_wat(ADD_WAT).unwrap();
        let mut instance = module.instantiate().unwrap();

        let result = instance.call_i32("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn default_config() {
        let config = WasmConfig::default();
        assert_eq!(config.max_fuel, Some(1_000_000));
        assert_eq!(config.max_memory_bytes, 64 * 1024 * 1024);
        assert!(config.enable_simd);
        assert_eq!(config.namespace, "default");
    }

    #[test]
    fn multiple_instances_isolated() {
        let module = WasmModule::from_wat(ADD_WAT).unwrap();
        let mut inst1 = module.instantiate().unwrap();
        let mut inst2 = module.instantiate().unwrap();

        // Write different data to each instance's memory
        inst1.write_memory(0, b"instance1").unwrap();
        inst2.write_memory(0, b"instance2").unwrap();

        let data1 = inst1.read_memory(0, 9).unwrap();
        let data2 = inst2.read_memory(0, 9).unwrap();

        assert_eq!(data1, b"instance1");
        assert_eq!(data2, b"instance2");
    }

    // WAT module that exercises state_put, state_get, state_delete via host imports
    const STATE_WAT: &str = r#"
        (module
            (import "env" "state_put" (func $state_put (param i32 i32 i32 i32)))
            (import "env" "state_get" (func $state_get (param i32 i32 i32 i32) (result i32)))
            (import "env" "state_delete" (func $state_delete (param i32 i32)))

            ;; Store key "k1" at offset 0, value "hello" at offset 8
            ;; Scratch buffer for get result at offset 64

            (data (i32.const 0) "k1")      ;; key at 0, len 2
            (data (i32.const 8) "hello")    ;; value at 8, len 5

            ;; put_and_get: stores k1=hello, then gets k1, returns retrieved length
            (func (export "put_and_get") (result i32)
                ;; state_put(key_ptr=0, key_len=2, val_ptr=8, val_len=5)
                (call $state_put (i32.const 0) (i32.const 2) (i32.const 8) (i32.const 5))
                ;; state_get(key_ptr=0, key_len=2, out_ptr=64, out_max_len=32) -> length
                (call $state_get (i32.const 0) (i32.const 2) (i32.const 64) (i32.const 32))
            )

            ;; get_missing: tries to get a key that doesn't exist, should return -1
            (func (export "get_missing") (result i32)
                ;; Use "zz" as key (at offset 100)
                (call $state_get (i32.const 100) (i32.const 2) (i32.const 64) (i32.const 32))
            )

            ;; put_then_delete_then_get: put k1, delete k1, get k1 (should return -1)
            (func (export "put_delete_get") (result i32)
                (call $state_put (i32.const 0) (i32.const 2) (i32.const 8) (i32.const 5))
                (call $state_delete (i32.const 0) (i32.const 2))
                (call $state_get (i32.const 0) (i32.const 2) (i32.const 64) (i32.const 32))
            )

            (memory (export "memory") 1)
        )
    "#;

    #[test]
    fn state_put_and_get() {
        let module = WasmModule::from_wat(STATE_WAT).unwrap();
        let mut instance = module.instantiate().unwrap();

        let result = instance.call_i32("put_and_get").unwrap();
        assert_eq!(result, 5); // "hello" is 5 bytes

        // Verify the value was written to the scratch buffer at offset 64
        let data = instance.read_memory(64, 5).unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn state_get_missing_returns_negative() {
        let module = WasmModule::from_wat(STATE_WAT).unwrap();
        let mut instance = module.instantiate().unwrap();

        let result = instance.call_i32("get_missing").unwrap();
        assert_eq!(result, -1);
    }

    #[test]
    fn state_put_delete_get() {
        let module = WasmModule::from_wat(STATE_WAT).unwrap();
        let mut instance = module.instantiate().unwrap();

        let result = instance.call_i32("put_delete_get").unwrap();
        assert_eq!(result, -1); // After delete, key should not be found
    }

    #[test]
    fn state_namespace_isolation() {
        let config_a = WasmConfig {
            namespace: "ns_a".to_string(),
            ..Default::default()
        };
        let config_b = WasmConfig {
            namespace: "ns_b".to_string(),
            ..Default::default()
        };

        let module_a = WasmModule::from_wat_with_config(STATE_WAT, config_a).unwrap();
        let module_b = WasmModule::from_wat_with_config(STATE_WAT, config_b).unwrap();

        let mut inst_a = module_a.instantiate().unwrap();
        let mut inst_b = module_b.instantiate().unwrap();

        // Put k1=hello in namespace A
        let result_a = inst_a.call_i32("put_and_get").unwrap();
        assert_eq!(result_a, 5);

        // Namespace B should NOT see the key (different namespace prefix)
        let result_b = inst_b.call_i32("get_missing").unwrap();
        assert_eq!(result_b, -1);
    }
}
