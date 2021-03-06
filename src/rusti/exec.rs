// Copyright 2014 Murarth
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Rust code parsing and compilation.

extern crate rustc_driver;

use std::c_str::CString;
use std::io::fs::PathExtensions;
use std::io::util::NullWriter;
use std::mem::transmute;
use std::os::{getenv_as_bytes, split_paths};
use std::thread::Builder;

use super::rustc;
use super::rustc::llvm;
use super::rustc::metadata::cstore::RequireDynamic;
use super::rustc::middle::ty;
use super::rustc::session::config::{mod, basic_options, build_configuration, Options};
use super::rustc::session::config::Input;
use super::rustc::session::build_session;
use self::rustc_driver::driver;

use super::syntax::ast_map;
use super::syntax::diagnostics::registry::Registry;

// This seems like a such a simple solution that I'm surprised it works.
#[link(name = "morestack")]
extern "C" {
    fn __morestack();
}

fn morestack_addr() -> *const () {
    unsafe { transmute(__morestack) }
}

/// Compiles input code into an execution environment.
pub struct ExecutionEngine {
    ee: llvm::ExecutionEngineRef,
    modules: Vec<llvm::ModuleRef>,
    /// Additional search paths for libraries
    lib_paths: Vec<String>,
    sysroot: Path,
}

/// A value that can be translated into `ExecutionEngine` input
pub trait IntoInput {
    fn into_input(self) -> Input;
}

impl<'a> IntoInput for &'a str {
    fn into_input(self) -> Input {
        Input::Str(self.to_string())
    }
}

impl IntoInput for String {
    fn into_input(self) -> Input {
        Input::Str(self)
    }
}

impl IntoInput for Path {
    fn into_input(self) -> Input {
        Input::File(self)
    }
}

type Deps = Vec<Path>;

impl ExecutionEngine {
    /// Constructs a new `ExecutionEngine` with the given library search paths.
    pub fn new(libs: Vec<String>) -> ExecutionEngine {
        ExecutionEngine::new_with_input(String::new(), libs)
    }

    /// Constructs a new `ExecutionEngine` with the given starting input
    /// and library search paths.
    pub fn new_with_input<T>(input: T, libs: Vec<String>) -> ExecutionEngine
            where T: IntoInput {
        let sysroot = get_sysroot();

        let (llmod, deps) = compile_input(input.into_input(),
            sysroot.clone(), libs.clone())
            .expect("ExecutionEngine init input failed to compile");

        let morestack = morestack_addr();

        assert!(!morestack.is_null());

        let mm = unsafe { llvm::LLVMRustCreateJITMemoryManager(morestack) };

        assert!(!mm.is_null());

        let ee = unsafe { llvm::LLVMBuildExecutionEngine(llmod, mm) };

        if ee.is_null() {
            panic!("Failed to create ExecutionEngine: {}", llvm_error());
        }

        let ee = ExecutionEngine{
            ee: ee,
            modules: vec![llmod],
            lib_paths: libs,
            sysroot: sysroot,
        };

        ee.load_deps(&deps);

        ee
    }

    /// Compile a module and add it to the execution engine.
    /// If the module fails to compile, errors will be printed to `stderr`
    /// and `None` will be returned. Otherwise, the module is returned.
    pub fn add_module<T>(&mut self, input: T) -> Option<llvm::ModuleRef>
            where T: IntoInput {
        debug!("compiling module");

        let (llmod, deps) = match compile_input(input.into_input(),
                self.sysroot.clone(), self.lib_paths.clone()) {
            Some(r) => r,
            None => return None,
        };

        self.load_deps(&deps);

        self.modules.push(llmod);

        unsafe { llvm::LLVMExecutionEngineAddModule(self.ee, llmod); }

        Some(llmod)
    }

    /// Remove the given module from the execution engine.
    /// The module is destroyed after it is removed.
    ///
    /// # Panics
    ///
    /// If the Module does not exist within this `ExecutionEngine`.
    pub fn remove_module(&mut self, llmod: llvm::ModuleRef) {
        match self.modules.iter().position(|p| *p == llmod) {
            Some(i) => {
                self.modules.remove(i);
                let res = unsafe {
                    llvm::LLVMExecutionEngineRemoveModule(self.ee, llmod)
                };

                assert_eq!(res, 1);

                unsafe { llvm::LLVMDisposeModule(llmod) };
            },
            None => panic!("Module not contained in ExecutionEngine"),
        }
    }

    /// Compiles the given input only up to the analysis phase, calling the
    /// given closure with a borrowed reference to the analysis result.
    pub fn with_analysis<F, R, T>(&self, input: T, f: F) -> Option<R>
            where F: Send, R: Send, T: IntoInput,
            F: for<'tcx> FnOnce(&ty::CrateAnalysis<'tcx>) -> R {
        with_analysis(f, input.into_input(),
            self.sysroot.clone(), self.lib_paths.clone())
    }

    /// Searches for the named function in the set of loaded modules,
    /// beginning with the most recently added module.
    /// If the function is found, a raw pointer is returned.
    /// If the function is not found, `None` is returned.
    pub fn get_function(&mut self, name: &str) -> Option<*const ()> {
        name.with_c_str(|s| {
            for m in self.modules.iter().rev() {
                let fv = unsafe { llvm::LLVMGetNamedFunction(*m, s) };

                if !fv.is_null() {
                    let fp = unsafe { llvm::LLVMGetPointerToGlobal(self.ee, fv) };

                    assert!(!fp.is_null());

                    return Some(fp);
                }
            }

            None
        })
    }

    /// Searches for the named global in the set of loaded modules,
    /// beginning with the most recently added module.
    /// If the global is found, a raw pointer is returned.
    /// If the global is not found, `None` is returned.
    pub fn get_global(&mut self, name: &str) -> Option<*const ()> {
        name.with_c_str(|s| {
            for m in self.modules.iter().rev() {
                let gv = unsafe { llvm::LLVMGetNamedGlobal(*m, s) };

                if !gv.is_null() {
                    let gp = unsafe { llvm::LLVMGetPointerToGlobal(self.ee, gv) };

                    assert!(!gp.is_null());

                    return Some(gp);
                }
            }

            None
        })
    }

    /// Loads all dependencies of compiled code.
    /// Expects a series of paths to dynamic library files.
    fn load_deps(&self, deps: &Deps) {
        for path in deps.iter() {
            debug!("loading crate {}", path.display());
            path.with_c_str(|s| {
                let res = unsafe { llvm::LLVMRustLoadDynamicLibrary(s) };

                if res == 0 {
                    panic!("Failed to load crate {}: {}",
                        s, llvm_error());
                }
            });
        }
    }
}

#[unsafe_destructor]
impl Drop for ExecutionEngine {
    fn drop(&mut self) {
        unsafe { llvm::LLVMDisposeExecutionEngine(self.ee) };
    }
}

/// Returns last error from LLVM wrapper code.
/// Should not be kept around longer than the next LLVM call.
fn llvm_error() -> CString {
    unsafe { CString::new(llvm::LLVMRustGetLastError() as *const i8, false) }
}

/// `rustc` uses its own executable path to derive the sysroot.
/// Because we're not `rustc`, we have to go looking for the sysroot.
///
/// To do this, we search the directories in the `PATH` environment variable
/// for a file named `rustc` (`rustc.exe` on Windows). Upon finding it,
/// we use the parent directory of that directory as the sysroot.
///
/// e.g. if `/usr/local/bin` is in `PATH` and `/usr/local/bin/rustc` is found,
/// `/usr/local` will be the sysroot.
fn get_sysroot() -> Path {
    if let Some(path) = getenv_as_bytes("PATH") {
        let rustc = if cfg!(windows) { "rustc.exe" } else { "rustc" };

        debug!("searching for sysroot in PATH {}",
            String::from_utf8_lossy(path.as_slice()));

        for mut p in split_paths(path).into_iter() {
            if p.join(rustc).is_file() {
                debug!("sysroot from PATH entry {}", p.display());
                p.pop();
                return p;
            }
        }
    }

    panic!("Could not find sysroot");
}

fn build_exec_options(sysroot: Path, libs: Vec<String>) -> Options {
    let mut opts = basic_options();

    // librustc derives sysroot from the executable name.
    // Since we are not rustc, we must specify it.
    opts.maybe_sysroot = Some(sysroot);

    for p in libs.iter() {
        opts.search_paths.add_path(p.as_slice());
    }

    // Prefer faster build times
    opts.optimize = config::No;

    // Don't require a `main` function
    opts.crate_types = vec![config::CrateTypeDylib];

    opts
}

/// Compiles input up to phase 4, translation to LLVM.
///
/// Returns the LLVM `ModuleRef` and a series of paths to dynamic libraries
/// for crates used in the given input.
fn compile_input(input: Input, sysroot: Path, libs: Vec<String>)
        -> Option<(llvm::ModuleRef, Deps)> {
    // Eliminates the useless "task '<...>' panicked" message
    let task = Builder::new().stderr(box NullWriter);

    let res = task.spawn(move || {
        let opts = build_exec_options(sysroot, libs);
        let sess = build_session(opts, None, Registry::new(&rustc::DIAGNOSTICS));

        let cfg = build_configuration(&sess);

        let id = "repl".to_string();

        let krate = driver::phase_1_parse_input(&sess, cfg, &input);

        let krate = driver::phase_2_configure_and_expand(&sess, krate,
            id.as_slice(), None).expect("phase_2 returned `None`");

        let mut forest = ast_map::Forest::new(krate);
        let ast_map = driver::assign_node_ids_and_map(&sess, &mut forest);

        let arenas = ty::CtxtArenas::new();

        let analysis = driver::phase_3_run_analysis_passes(sess, ast_map, &arenas, id);

        let (tcx, trans) = driver::phase_4_translate_to_llvm(analysis);

        let crates = tcx.sess.cstore.get_used_crates(RequireDynamic);

        // Collect crates used in the session.
        // Reverse order finds dependencies first.
        let deps = crates.into_iter().rev()
            .filter_map(|(_, p)| p).collect();

        assert_eq!(trans.modules.len(), 1);
        let llmod = trans.modules[0].llmod;

        // Workaround because raw pointers do not impl Send
        let modp: uint = unsafe { transmute(llmod) };

        (modp, deps)
    }).join();

    match res {
        Ok((llmod, deps)) => Some((unsafe { transmute(llmod) }, deps)),
        Err(_) => None,
    }
}

/// Compiles input up to phase 3, type/region check analysis, and calls
/// the given closure with the resulting `CrateAnalysis`.
fn with_analysis<F, R>(f: F, input: Input, sysroot: Path, libs: Vec<String>) -> Option<R>
        where F: Send, R: Send,
        F: for<'tcx> FnOnce(&ty::CrateAnalysis<'tcx>) -> R {
    // Eliminates the useless "task '<...>' panicked" message
    let task = Builder::new().stderr(box NullWriter);

    let res = task.spawn(move || {
        let opts = build_exec_options(sysroot, libs);
        let sess = build_session(opts, None, Registry::new(&rustc::DIAGNOSTICS));

        let cfg = build_configuration(&sess);

        let id = "repl".to_string();

        let krate = driver::phase_1_parse_input(&sess, cfg, &input);

        let krate = driver::phase_2_configure_and_expand(&sess, krate,
            id.as_slice(), None).expect("phase_2 returned `None`");

        let mut forest = ast_map::Forest::new(krate);
        let ast_map = driver::assign_node_ids_and_map(&sess, &mut forest);

        let arenas = ty::CtxtArenas::new();

        let analysis = driver::phase_3_run_analysis_passes(sess, ast_map, &arenas, id);

        f(&analysis)
    }).join();

    res.ok()
}
