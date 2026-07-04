//! # dadhichi-wasm
//!
//! A **sandboxed WASM plugin runtime**. Plugins are WebAssembly modules run
//! under [`wasmi`] with three isolation properties that make untrusted code
//! safe to host:
//!
//! - **No ambient authority.** A module can only touch the host through the
//!   functions the runtime explicitly links — the capability boundary. This one
//!   links a single `env::host_log`; a real host links only what the plugin's
//!   manifest requested and the user granted.
//! - **Fuel metering.** Every instruction costs fuel; a runaway or malicious
//!   plugin traps with [`WasmError::OutOfFuel`] instead of hanging the IDE.
//! - **Memory isolation.** The module runs in its own linear memory; it cannot
//!   reach host memory except through the linked functions.
//!
//! This uses `wasmi` (a portable interpreter) for a fast, dependency-light
//! build; the production target is a JIT (`wasmtime`) with full WASI, behind the
//! same [`WasmRuntime`] surface.
//!
//! ```
//! use dadhichi_wasm::WasmRuntime;
//!
//! // (module (func (export "add") (param i32 i32) (result i32)
//! //   local.get 0 local.get 1 i32.add))
//! let wasm = wat::parse_str(
//!     r#"(module (func (export "add") (param i32 i32) (result i32)
//!          local.get 0 local.get 1 i32.add))"#,
//! ).unwrap();
//!
//! let runtime = WasmRuntime::new();
//! let mut plugin = runtime.instantiate(&wasm, 1_000_000).unwrap();
//! assert_eq!(plugin.call_ii_i("add", 2, 3).unwrap(), 5);
//! ```

use thiserror::Error;
use wasmi::{Caller, Config, Engine, Linker, Module, Store};

/// Errors from loading or running a plugin.
#[derive(Debug, Error)]
pub enum WasmError {
    /// The module bytes were not valid WebAssembly.
    #[error("invalid module: {0}")]
    InvalidModule(String),
    /// Instantiation (linking imports, running start) failed.
    #[error("instantiation failed: {0}")]
    Instantiation(String),
    /// The requested export does not exist or has the wrong signature.
    #[error("no such export: {0}")]
    NoExport(String),
    /// The plugin exhausted its fuel budget and was trapped.
    #[error("plugin exhausted its fuel budget")]
    OutOfFuel,
    /// The plugin trapped for another reason.
    #[error("plugin trapped: {0}")]
    Trap(String),
}

/// Host state a plugin can affect through linked functions.
#[derive(Debug, Default)]
pub struct HostState {
    /// Values the plugin passed to `env::host_log`.
    pub logs: Vec<i64>,
}

/// A runtime that instantiates and runs WASM plugins.
#[derive(Debug)]
pub struct WasmRuntime {
    engine: Engine,
}

impl Default for WasmRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmRuntime {
    /// Create a runtime with fuel metering enabled.
    pub fn new() -> Self {
        let mut config = Config::default();
        config.consume_fuel(true);
        Self {
            engine: Engine::new(&config),
        }
    }

    /// Instantiate `wasm` with a `fuel` budget, linking the host capability API.
    pub fn instantiate(&self, wasm: &[u8], fuel: u64) -> Result<WasmPlugin, WasmError> {
        let module =
            Module::new(&self.engine, wasm).map_err(|e| WasmError::InvalidModule(e.to_string()))?;
        let mut store = Store::new(&self.engine, HostState::default());
        store
            .set_fuel(fuel)
            .map_err(|e| WasmError::Instantiation(e.to_string()))?;

        let mut linker = Linker::new(&self.engine);
        // The single linked capability: recording a value. A production host
        // gates each linked function on a granted capability.
        linker
            .func_wrap(
                "env",
                "host_log",
                |mut caller: Caller<'_, HostState>, value: i64| {
                    caller.data_mut().logs.push(value);
                },
            )
            .map_err(|e| WasmError::Instantiation(e.to_string()))?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| WasmError::Instantiation(e.to_string()))?
            .start(&mut store)
            .map_err(|e| WasmError::Instantiation(e.to_string()))?;

        Ok(WasmPlugin { store, instance })
    }
}

/// A live, instantiated plugin.
pub struct WasmPlugin {
    store: Store<HostState>,
    instance: wasmi::Instance,
}

impl std::fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("fuel", &self.store.get_fuel().ok())
            .finish_non_exhaustive()
    }
}

impl WasmPlugin {
    /// Call an exported `(i32, i32) -> i32` function.
    pub fn call_ii_i(&mut self, name: &str, a: i32, b: i32) -> Result<i32, WasmError> {
        let func = self
            .instance
            .get_typed_func::<(i32, i32), i32>(&self.store, name)
            .map_err(|_| WasmError::NoExport(name.to_string()))?;
        func.call(&mut self.store, (a, b))
            .map_err(|e| self.map_trap(e))
    }

    /// Call an exported `(i32) -> i32` function.
    pub fn call_i_i(&mut self, name: &str, a: i32) -> Result<i32, WasmError> {
        let func = self
            .instance
            .get_typed_func::<i32, i32>(&self.store, name)
            .map_err(|_| WasmError::NoExport(name.to_string()))?;
        func.call(&mut self.store, a).map_err(|e| self.map_trap(e))
    }

    /// Call an exported `() -> ()` function.
    pub fn call_void(&mut self, name: &str) -> Result<(), WasmError> {
        let func = self
            .instance
            .get_typed_func::<(), ()>(&self.store, name)
            .map_err(|_| WasmError::NoExport(name.to_string()))?;
        func.call(&mut self.store, ()).map_err(|e| self.map_trap(e))
    }

    /// Values the plugin recorded through `env::host_log`.
    pub fn logs(&self) -> &[i64] {
        &self.store.data().logs
    }

    /// Remaining fuel, if metering is on.
    pub fn remaining_fuel(&self) -> Option<u64> {
        self.store.get_fuel().ok()
    }

    fn map_trap(&self, err: wasmi::Error) -> WasmError {
        let text = err.to_string();
        if text.contains("fuel") || text.contains("OutOfFuel") {
            WasmError::OutOfFuel
        } else {
            WasmError::Trap(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(wat: &str) -> Vec<u8> {
        wat::parse_str(wat).expect("valid wat")
    }

    #[test]
    fn runs_a_pure_function() {
        let wasm = compile(
            r#"(module (func (export "add") (param i32 i32) (result i32)
                 local.get 0 local.get 1 i32.add))"#,
        );
        let mut plugin = WasmRuntime::new().instantiate(&wasm, 1_000_000).unwrap();
        assert_eq!(plugin.call_ii_i("add", 40, 2).unwrap(), 42);
        assert!(
            plugin.remaining_fuel().unwrap() < 1_000_000,
            "fuel was consumed"
        );
    }

    #[test]
    fn plugin_reaches_host_only_through_linked_functions() {
        let wasm = compile(
            r#"(module
                 (import "env" "host_log" (func $log (param i64)))
                 (func (export "run") (param i32) (result i32)
                   (call $log (i64.extend_i32_s (local.get 0)))
                   (local.get 0)))"#,
        );
        let mut plugin = WasmRuntime::new().instantiate(&wasm, 1_000_000).unwrap();
        assert_eq!(plugin.call_i_i("run", 7).unwrap(), 7);
        assert_eq!(plugin.logs(), &[7]);
    }

    #[test]
    fn a_runaway_plugin_runs_out_of_fuel() {
        let wasm = compile(r#"(module (func (export "spin") (loop (br 0))))"#);
        // A tight fuel budget: the infinite loop must trap rather than hang.
        let mut plugin = WasmRuntime::new().instantiate(&wasm, 10_000).unwrap();
        assert!(matches!(
            plugin.call_void("spin"),
            Err(WasmError::OutOfFuel)
        ));
    }

    #[test]
    fn importing_an_ungranted_function_fails_to_instantiate() {
        // The module imports a function the runtime does not link → rejected.
        let wasm = compile(
            r#"(module (import "env" "danger" (func (param i32)))
                 (func (export "noop")))"#,
        );
        let err = WasmRuntime::new().instantiate(&wasm, 1000).unwrap_err();
        assert!(matches!(err, WasmError::Instantiation(_)));
    }

    #[test]
    fn invalid_bytes_are_rejected() {
        let err = WasmRuntime::new()
            .instantiate(b"not wasm", 1000)
            .unwrap_err();
        assert!(matches!(err, WasmError::InvalidModule(_)));
    }
}
