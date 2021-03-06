// Copyright 2014 Murarth
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Runs Rust code in an encapsulated environment

use std::io::File;
use std::io::stdio::stdin_raw;
use std::mem::transmute;
use std::os;

use super::exec::ExecutionEngine;
use super::input::{parse_command, parse_program};
use super::input::{FileReader, Input, InputReader, ViewItem};
use super::input::InputResult::*;

use super::rustc::middle::ty;
use super::rustc::util::ppaux::Repr;

use super::syntax::{ast, codemap, visit};
use super::syntax::ast::Stmt_::StmtSemi;
use super::syntax::parse::token;

/// Starting prompt
const DEFAULT_PROMPT: &'static str = "rusti=> ";
/// Prompt when further input is being read
const MORE_PROMPT: &'static str = "rusti.> ";
/// Prompt when a `.block` command is in effect
const BLOCK_PROMPT: &'static str = "rusti+> ";

// TODO: Implement commands:
//     def <name>; shows the definition of type or fn
//     doc <name>; links to rustdoc page for name
//     help; lists commands and their uses

/// List of command names
static COMMANDS: &'static [&'static str] = &[
    "block",
    "type",
];

/// Executes input code and maintains state of persistent items.
pub struct Repl {
    engine: ExecutionEngine,
    /// Module-level attributes applied to every program
    attributes: Vec<String>,
    /// View items compiled into every program
    view_items: Vec<(ViewItem, String)>,
    /// Items compiled into every program
    /// TODO: When type/def-injection is implemented,
    /// it will not be necessary to re-compile all functions on every input.
    items: Vec<String>,
    /// true if the next input should be a block
    read_block: bool,
}

/// Looks up a command name by what may be an abbreviated prefix.
/// Returns the full command name. e.g. `"b"` => `Some("block")`
fn lookup_command(name: &str) -> Option<&'static str> {
    for cmd in COMMANDS.iter() {
        if cmd.starts_with(name) {
            return Some(*cmd);
        }
    }
    None
}

impl Repl {
    /// Constructs a new `Repl`.
    pub fn new() -> Repl {
        Repl::new_with_libs(Vec::new())
    }

    /// Constructs a new `Repl` with additional library lookup paths.
    pub fn new_with_libs(libs: Vec<String>) -> Repl {
        Repl{
            engine: ExecutionEngine::new(libs),
            attributes: Vec::new(),
            view_items: Vec::new(),
            items: Vec::new(),
            read_block: false,
        }
    }

    /// Evaluates a single round of input, printing the result to `stdout`.
    pub fn eval(&mut self, input: &str) {
        match parse_program(input, false, None) {
            Program(i) => self.handle_input(i),
            _ => (),
        }
    }

    /// Runs the REPL interactively.
    pub fn run(&mut self) {
        let mut more = false;
        let mut input = InputReader::new();

        loop {
            let res = if self.read_block {
                self.read_block = false;
                input.read_block_input(BLOCK_PROMPT)
            } else {
                input.read_input(if more { MORE_PROMPT } else { DEFAULT_PROMPT })
            };

            match res {
                Command(name, args) => {
                    debug!("read command: {} {}", name, args);

                    self.handle_command(name, args);
                },
                Program(input) => {
                    debug!("read program: {}", input);

                    more = false;
                    self.handle_input(input);
                },
                Empty => (),
                More => { more = true; },
                Eof => {
                    if stdin_raw().isatty() {
                        println!("");
                    }
                    break;
                }
                InputError(err) => {
                    if let Some(err) = err {
                        println!("{}", err);
                    }
                    more = false;
                },
            };
        }
    }

    /// Runs a single `rusti` command.
    pub fn run_command(&mut self, cmd: &str) {
        match parse_command(cmd) {
            Command(name, args) => self.handle_command(name, args),
            InputError(Some(err)) => println!("{}", err),
            _ => ()
        }
    }

    /// Runs rusti input from the named file.
    /// Returns `true` if it was compiled successfully.
    pub fn run_file(&mut self, path: Path) -> bool {
        let f = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                println!("{}: {}", os::args()[0], e);
                return false;
            }
        };

        let mut input = FileReader::new(f);

        loop {
            if self.read_block {
                println!("{}: `.block` command is not necessary when running a file",
                    os::args()[0]);
                return false;
            }

            let input = input.read_input();

            match input {
                Program(input) => self.handle_input(input),
                Command(name, args) => self.handle_command(name, args),
                InputError(Some(e)) => {
                    println!("{}: {}", os::args()[0], e);
                    return false;
                }
                InputError(None) => return false,
                Eof => break,
                _ => unreachable!(),
            }
        }

        true
    }

    /// Build a program text containing all persistent items seen so far and,
    /// optionally, those from an `Input` instance. The `statements` field of
    /// `input` will be ignored.
    fn build_program(&self, input: Option<&Input>, program: &str) -> String {
        let (attrs, vitems, items) = if let Some(input) = input {
            let attrs = self.attributes.iter().map(|s| s.as_slice())
                .chain(input.attributes.iter().map(|s| s.as_slice()))
                .collect::<Vec<_>>();

            let mut vitems = self.view_items.iter().map(|&(a, ref b)| (a, b.as_slice()))
                .chain(input.view_items.iter().map(|&(a, ref b)| (a, b.as_slice())))
                .collect::<Vec<_>>();

            // Sort `extern crate` before `use`
            vitems.sort_by(|&(a, _), &(b, _)| a.cmp(&b));

            let items = self.items.iter().map(|s| s.as_slice())
                .chain(input.items.iter().map(|s| s.as_slice()))
                .collect::<Vec<_>>();

            (attrs, vitems, items)
        } else {
            let attrs = self.attributes.iter().map(|s| s.as_slice())
                .collect::<Vec<_>>();

            let mut vitems = self.view_items.iter().map(|&(a, ref b)| (a, b.as_slice()))
                .collect::<Vec<_>>();

            // Sort `extern crate` before `use`
            vitems.sort_by(|&(a, _), &(b, _)| a.cmp(&b));

            let items = self.items.iter().map(|s| s.as_slice())
                .collect::<Vec<_>>();

            (attrs, vitems, items)
        };

        let attrs = attrs.connect("\n");
        let vitems = vitems.iter().map(|&(_, s)| s)
            .collect::<Vec<_>>().connect("\n");
        let items = items.connect("\n");

        format!(
r#"#![allow(dead_code, unused_imports)]
{attrs}
{vitems}
{items}
{program}
"#
        , attrs = attrs
        , vitems = vitems
        , items = items
        , program = program)
    }

    /// Runs a single command input.
    fn handle_command(&mut self, cmd: String, args: Option<String>) {
        match lookup_command(cmd.as_slice()) {
            Some("block") => {
                if args.is_some() {
                    println!("command `block` takes no arguments");
                } else {
                    self.read_block = true;
                }
            },
            Some("type") => {
                if let Some(args) = args {
                    self.type_command(args);
                } else {
                    println!("command `type` expects an expression");
                }
            },
            _ => println!("unrecognized command `{}`", cmd),
        }
    }

    /// Runs a single program input.
    fn handle_input(&mut self, mut input: Input) {
        let name = "_rusti_run";

        if input.last_expr && !input.statements.is_empty() {
            let stmt = input.statements.last_mut().unwrap();
            *stmt = format!(r#"println!("{{}}", {{ {} }});"#, stmt);
        }

        let stmts = input.statements.connect("\n");

        let prog = self.build_program(Some(&input),
            format!(
r#"
#[no_mangle]
pub fn {name}() {{
    let _ = unsafe {{ std::rt::unwind::try(_rusti_inner) }};
}}

fn _rusti_inner() {{
{stmts}
}}
"#
            , name = name
            , stmts = stmts
            ).as_slice()
        );

        if let Some(_) = self.engine.add_module(prog) {
            let fp = self.engine.get_function(name).unwrap();
            let f: fn() = unsafe { transmute(fp) };

            f();

            // NOTE: The module cannot be removed after it is run because tasks
            // may still be running in the module code. This means that rusti's
            // memory footprint will only grow over time.
            // Hopefully, this will not be noticeable in normal use.

            // Successful compile means we can add the new items to every program
            self.attributes.extend(input.attributes.into_iter());
            self.view_items.extend(input.view_items.into_iter());
            self.items.extend(input.items.into_iter());
        }
    }

    fn expr_type(&self, fn_name: &str, prog: String) -> Option<String> {
        let fn_name = fn_name.to_string();

        self.engine.with_analysis(prog, move |analysis| {
            let mut v = ExprType{
                fn_name: fn_name,
                result: None,
                ty_cx: &analysis.ty_cx,
            };

            visit::walk_crate(&mut v, analysis.ty_cx.map.krate());

            if let Some(ty) = v.result {
                ty
            } else {
                panic!("no type found");
            }
        })
    }

    fn type_command(&mut self, expr: String) {
        let name = "_rusti_type";
        let prog = self.build_program(None, format!(
r#"
fn {name}() {{
{expr}
}}
"#
        , name = name
        , expr = format!("{{ {} }};", expr)
        ).as_slice());

        if let Some(t) = self.expr_type(name, prog) {
            println!("{} = {}", expr, t);
        }
    }
}

struct ExprType<'a, 'tcx: 'a> {
    fn_name: String,
    result: Option<String>,
    ty_cx: &'a ty::ctxt<'tcx>,
}

impl<'v, 'a, 'tcx> visit::Visitor<'v> for ExprType<'a, 'tcx> {
    fn visit_fn(&mut self, fk: visit::FnKind<'v>, _fd: &'v ast::FnDecl,
            b: &'v ast::Block, _s: codemap::Span, _n: ast::NodeId) {
        if let visit::FkItemFn(ident, _, _, _) = fk {
            if token::get_ident(ident).get() == self.fn_name {
                if let Some(ref stmt) = b.stmts.last() {
                    if let StmtSemi(ref expr, _) = stmt.node {
                        let id = expr.id;
                        if let Some(ty) = self.ty_cx.node_types.borrow().get(&id) {
                            self.result = Some(ty.repr(self.ty_cx));
                        }
                    }
                }
            }
        }
    }
}
