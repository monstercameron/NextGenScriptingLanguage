//! Small owned-buffer ABI. All interpreter and session state stays in Rust/WASM.
use flow::runtime::{BrowserSession, Request};
use std::cell::RefCell;

thread_local! { static SESSION: RefCell<BrowserSession> = RefCell::new(BrowserSession::default()); }
const MAX_REQUEST: usize = 8 * 1024 * 1024;

#[no_mangle]
pub extern "C" fn allocate(length: usize) -> *mut u8 {
    if length > MAX_REQUEST {
        return std::ptr::null_mut();
    }
    Box::into_raw(vec![0u8; length].into_boxed_slice()) as *mut u8
}

/// # Safety
/// `pointer,length` must be an unfreed allocation returned by allocate or execute.
#[no_mangle]
pub unsafe extern "C" fn release(pointer: *mut u8, length: usize) {
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
        pointer, length,
    )));
}

/// Consumes its input allocation and returns an owned output (pointer << 32 | length).
/// # Safety
/// Input must be an initialized allocation returned by allocate, used once.
#[no_mangle]
pub unsafe extern "C" fn execute(pointer: *mut u8, length: usize) -> u64 {
    let bytes = Box::from_raw(std::ptr::slice_from_raw_parts_mut(pointer, length));
    let result = match serde_json::from_slice::<Request>(&bytes) {
        Ok(request) => SESSION
            .with(|session| serde_json::to_vec(&session.borrow_mut().handle(request)).unwrap()),
        Err(error) => {
            serde_json::to_vec(&serde_json::json!({"ok":false,"error":error.to_string()})).unwrap()
        }
    };
    let length = result.len();
    let pointer = Box::into_raw(result.into_boxed_slice()) as *mut u8;
    ((pointer as u64) << 32) | length as u64
}
