extern crate rusti;

use std::mem::transmute;

use rusti::exec::ExecutionEngine;

fn new_ee(code: &str) -> ExecutionEngine {
    ExecutionEngine::new_with_input(code, Vec::new())
}

#[ignore]
#[test]
fn test_exec() {
    let mut ee = new_ee(
r#"
#[no_mangle]
pub fn hello() -> int {
    123
}
"#);

    let f: fn() -> int = unsafe { transmute(ee.get_function("hello")
        .expect("could not get fn hello")) };

    assert_eq!(f(), 123);
}

#[ignore]
#[test]
fn test_static() {
    let mut ee = new_ee(
r#"
#[no_mangle]
pub static FOO: int = 12345;

#[no_mangle]
pub fn get_foo() -> int {
    FOO
}
"#);

    let foo_var: *const int = unsafe { transmute(ee.get_global("FOO")
        .expect("could not get static FOO")) };

    assert_eq!(unsafe { *foo_var }, 12345);

    let foo_fn: fn() -> int = unsafe { transmute(ee.get_function("get_foo")
        .expect("could not get fn get_foo")) };

    assert_eq!(foo_fn(), 12345);
}

#[ignore]
#[test]
fn test_static_mut() {
    let mut ee = new_ee(
r#"
static mut FOO: int = 1;

#[no_mangle]
pub fn set_foo(i: int) {
    unsafe { FOO = i; }
}

#[no_mangle]
pub fn get_foo() -> int {
    unsafe { FOO }
}
"#);

    let get: fn() -> int = unsafe { transmute(ee.get_function("get_foo")
        .expect("could not get fn get_foo")) };
    let set: fn(int) = unsafe { transmute(ee.get_function("set_foo")
        .expect("could not get fn set_foo")) };

    assert_eq!(get(), 1);

    set(123);
    assert_eq!(get(), 123);

    set(456);
    assert_eq!(get(), 456);
}

// LLVM fails with "Relocation type not implemented yet!"
#[ignore]
#[test]
fn test_thread_local() {
    let mut ee = new_ee(
r#"
use std::cell::RefCell;

#[no_mangle]
pub fn thread_local() -> int {
    thread_local!(static FOO: RefCell<int> = RefCell::new(123));
    FOO.with(|k| *k.borrow_mut() = 456);
    FOO.with(|k| *k.borrow())
}
"#);
    
    let f: fn() -> int = unsafe { transmute(ee.get_function("thread_local")
        .expect("could not get fn thread_local")) };

    assert_eq!(f(), 123);
}

#[ignore]
#[test]
fn test_thread() {
    let mut ee = new_ee(
r#"
#[no_mangle]
pub fn thread_spawn() {
    let _ = std::thread::Thread::spawn(|| ()).join();
}
"#);

    let f: fn() = unsafe { transmute(ee.get_function("thread_spawn")
        .expect("could not get fn thread_spawn")) };

    f();
}
