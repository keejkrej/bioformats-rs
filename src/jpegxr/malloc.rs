//! WASM allocator bridge for the vendored C JXRLib implementation.
//!
//! The exported functions are only built for WASM. The implementation helpers
//! remain available to native unit tests so their overflow, alignment, and
//! reallocation contracts can be exercised without colliding with the host
//! libc's allocator symbols.

use std::alloc::{alloc, alloc_zeroed, dealloc, realloc as alloc_realloc, Layout};
use std::ffi::c_void;
use std::mem::{align_of, size_of};
use std::ptr;

// Clang's wasm32 ABI gives max_align_t 16-byte alignment. Keeping the header
// itself at that alignment makes both the allocation base and the returned C
// payload suitable for any fundamental C type JXRLib can place there.
#[repr(C, align(16))]
struct AllocationHeader {
    allocation_size: usize,
}

const ALLOCATION_ALIGNMENT: usize = align_of::<AllocationHeader>();
const HEADER_SIZE: usize = size_of::<AllocationHeader>();

fn allocation_layout(requested_size: usize) -> Option<Layout> {
    // C permits malloc(0) and calloc(0, n) to return a unique, freeable
    // pointer. Reserving one payload byte gives that behavior without ever
    // asking Rust's allocator for a zero-sized allocation.
    let payload_size = requested_size.max(1);
    let allocation_size = HEADER_SIZE.checked_add(payload_size)?;
    Layout::from_size_align(allocation_size, ALLOCATION_ALIGNMENT).ok()
}

fn calloc_size(items: usize, item_size: usize) -> Option<usize> {
    items.checked_mul(item_size)
}

unsafe fn allocate_payload(requested_size: usize, zeroed: bool) -> *mut c_void {
    let Some(layout) = allocation_layout(requested_size) else {
        return ptr::null_mut();
    };
    let allocation = if zeroed {
        // SAFETY: layout is non-zero and valid for the global allocator.
        unsafe { alloc_zeroed(layout) }
    } else {
        // SAFETY: layout is non-zero and valid for the global allocator.
        unsafe { alloc(layout) }
    };
    if allocation.is_null() {
        return ptr::null_mut();
    }

    // SAFETY: allocation is aligned for AllocationHeader and owns layout.size
    // writable bytes. HEADER_SIZE is part of that allocation.
    unsafe {
        allocation
            .cast::<AllocationHeader>()
            .write(AllocationHeader {
                allocation_size: layout.size(),
            });
        allocation.add(HEADER_SIZE).cast::<c_void>()
    }
}

unsafe fn allocation_from_payload(payload: *mut c_void) -> (*mut u8, Layout) {
    // SAFETY: callers only pass non-null pointers previously returned by this
    // allocator, which have an AllocationHeader immediately before them.
    let allocation = unsafe { payload.cast::<u8>().sub(HEADER_SIZE) };
    // SAFETY: allocation points to an initialized, properly aligned header.
    let allocation_size = unsafe { allocation.cast::<AllocationHeader>().read().allocation_size };
    // SAFETY: allocation_size and alignment came from allocation_layout when
    // this allocation was created or successfully resized.
    let layout =
        unsafe { Layout::from_size_align_unchecked(allocation_size, ALLOCATION_ALIGNMENT) };
    (allocation, layout)
}

unsafe fn deallocate_payload(payload: *mut c_void) {
    if payload.is_null() {
        return;
    }
    // SAFETY: the non-null payload is required by C's free contract to have
    // been returned by this allocator and not already freed.
    let (allocation, layout) = unsafe { allocation_from_payload(payload) };
    // SAFETY: allocation and layout describe the live global allocation.
    unsafe { dealloc(allocation, layout) };
}

unsafe fn reallocate_payload(payload: *mut c_void, requested_size: usize) -> *mut c_void {
    if payload.is_null() {
        // SAFETY: allocation has no pointer preconditions.
        return unsafe { allocate_payload(requested_size, false) };
    }
    if requested_size == 0 {
        // C permits realloc(p, 0) to free p and return null. Choosing that
        // well-established behavior avoids manufacturing a replacement block.
        // SAFETY: the non-null payload satisfies deallocate_payload's contract.
        unsafe { deallocate_payload(payload) };
        return ptr::null_mut();
    }

    let Some(new_layout) = allocation_layout(requested_size) else {
        // realloc failure must leave the original allocation untouched.
        return ptr::null_mut();
    };
    // SAFETY: the non-null payload came from this allocator.
    let (allocation, old_layout) = unsafe { allocation_from_payload(payload) };
    // SAFETY: allocation and old_layout identify a live allocation, and the
    // new size is non-zero. Rust's realloc leaves it untouched on failure.
    let new_allocation = unsafe { alloc_realloc(allocation, old_layout, new_layout.size()) };
    if new_allocation.is_null() {
        return ptr::null_mut();
    }

    // SAFETY: a successful realloc returned new_layout.size writable bytes at
    // the same alignment, including space for the header.
    unsafe {
        new_allocation
            .cast::<AllocationHeader>()
            .write(AllocationHeader {
                allocation_size: new_layout.size(),
            });
        new_allocation.add(HEADER_SIZE).cast::<c_void>()
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    // SAFETY: allocation has no pointer preconditions.
    unsafe { allocate_payload(size, false) }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[no_mangle]
pub unsafe extern "C" fn calloc(items: usize, item_size: usize) -> *mut c_void {
    let Some(size) = calloc_size(items, item_size) else {
        return ptr::null_mut();
    };
    // SAFETY: allocation has no pointer preconditions.
    unsafe { allocate_payload(size, true) }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[no_mangle]
pub unsafe extern "C" fn realloc(payload: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: the C allocator contract requires a non-null payload to have
    // come from malloc/calloc/realloc and not already have been freed.
    unsafe { reallocate_payload(payload, size) }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[no_mangle]
pub unsafe extern "C" fn free(payload: *mut c_void) {
    // SAFETY: the C allocator contract requires a non-null payload to have
    // come from malloc/calloc/realloc and not already have been freed.
    unsafe { deallocate_payload(payload) }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::{
        allocate_payload, allocation_layout, calloc_size, deallocate_payload, reallocate_payload,
        ALLOCATION_ALIGNMENT, HEADER_SIZE,
    };

    #[test]
    fn layout_is_aligned_and_checked_for_zero_and_overflow() {
        assert!(ALLOCATION_ALIGNMENT >= 16);
        assert_eq!(HEADER_SIZE % ALLOCATION_ALIGNMENT, 0);

        let zero = allocation_layout(0).expect("zero-size request has a freeable layout");
        assert_eq!(zero.align(), ALLOCATION_ALIGNMENT);
        assert_eq!(zero.size(), HEADER_SIZE + 1);
        assert!(allocation_layout(usize::MAX).is_none());
    }

    #[test]
    fn calloc_arithmetic_rejects_overflow() {
        assert_eq!(calloc_size(0, usize::MAX), Some(0));
        assert_eq!(calloc_size(3, 7), Some(21));
        assert_eq!(calloc_size(usize::MAX, 2), None);
    }

    #[test]
    fn allocation_is_max_aligned_and_calloc_payload_is_zeroed() {
        let payload = unsafe { allocate_payload(13, true) };
        assert!(!payload.is_null());
        assert_eq!(payload as usize % ALLOCATION_ALIGNMENT, 0);

        let bytes = unsafe { std::slice::from_raw_parts(payload.cast::<u8>(), 13) };
        assert_eq!(bytes, &[0; 13]);
        unsafe { deallocate_payload(payload) };
    }

    #[test]
    fn zero_size_allocation_is_unique_and_freeable() {
        let first = unsafe { allocate_payload(0, false) };
        let second = unsafe { allocate_payload(0, false) };
        assert!(!first.is_null());
        assert!(!second.is_null());
        assert_ne!(first, second);
        unsafe {
            deallocate_payload(first);
            deallocate_payload(second);
        }
    }

    #[test]
    fn realloc_preserves_bytes_and_alignment() {
        let payload = unsafe { allocate_payload(8, false) };
        assert!(!payload.is_null());
        unsafe {
            for index in 0..8 {
                payload.cast::<u8>().add(index).write(index as u8);
            }
        }

        let grown = unsafe { reallocate_payload(payload, 64) };
        assert!(!grown.is_null());
        assert_eq!(grown as usize % ALLOCATION_ALIGNMENT, 0);
        let bytes = unsafe { std::slice::from_raw_parts(grown.cast::<u8>(), 8) };
        assert_eq!(bytes, &[0, 1, 2, 3, 4, 5, 6, 7]);
        unsafe { deallocate_payload(grown) };
    }

    #[test]
    fn failed_realloc_preserves_original_allocation() {
        let payload = unsafe { allocate_payload(4, false) };
        assert!(!payload.is_null());
        unsafe { payload.cast::<u8>().write_bytes(0x5a, 4) };

        let failed = unsafe { reallocate_payload(payload, usize::MAX) };
        assert!(failed.is_null());
        let bytes = unsafe { std::slice::from_raw_parts(payload.cast::<u8>(), 4) };
        assert_eq!(bytes, &[0x5a; 4]);
        unsafe { deallocate_payload(payload) };
    }

    #[test]
    fn realloc_handles_null_and_zero_size() {
        let from_null = unsafe { reallocate_payload(ptr::null_mut(), 0) };
        assert!(!from_null.is_null());
        unsafe { deallocate_payload(from_null) };

        let payload = unsafe { allocate_payload(4, false) };
        assert!(!payload.is_null());
        let zero = unsafe { reallocate_payload(payload, 0) };
        assert!(zero.is_null());
    }
}
