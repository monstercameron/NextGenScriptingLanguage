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
    // SAFETY: the `# Safety` contract above makes `pointer,length` an unfreed allocation that
    // allocate or execute returned as a boxed slice of exactly `length` bytes, so reclaiming it
    // as a Box<[u8]> here is the matching free and happens once.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(pointer, length)) });
}

/// Consumes its input allocation and returns an owned output (pointer << 32 | length).
/// # Safety
/// Input must be an initialized allocation returned by allocate, used once.
#[no_mangle]
pub unsafe extern "C" fn execute(pointer: *mut u8, length: usize) -> u64 {
    // SAFETY: the `# Safety` contract above makes `pointer,length` an initialized allocation
    // that allocate returned, and it is used once, so taking ownership of it as a Box<[u8]>
    // is sound and the input buffer is freed when `bytes` drops.
    let bytes = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(pointer, length)) };
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

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use flow::runtime::{BrowserSession, Request};

    #[test]
    fn result_binding_is_rejected_before_prior_virtual_file_writes() {
        let mut session = BrowserSession::default();
        let response = session.handle(Request {
            action: "run".into(),
            source: "run printf with message \"effect\"\nthen save to \"marker.txt\"\nrun printf with message \"result\" and keep result as execution\n".into(),
            ..Request::default()
        });

        assert!(!response.ok);
        assert!(response
            .error
            .contains("source-level execution result capture"));
        assert!(!response.files.contains_key("/workspace/marker.txt"));
        assert!(!response.written_files.contains("/workspace/marker.txt"));
    }
}
