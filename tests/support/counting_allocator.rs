//! The test binary's global allocator, which counts the allocations of the
//! thread asking so the modules that pin allocation behaviour can measure their
//! own thread alone.

struct CountingAllocator;

thread_local! {
    /// Allocations the current thread asked for.
    ///
    /// The `const` initializer keeps the slot from allocating when it is first
    /// touched, which would recurse through this allocator.
    static ALLOCATIONS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Records one allocation for the current thread, ignoring a slot already
/// destroyed during thread teardown.
fn record_allocation() {
    let _ = ALLOCATIONS.try_with(|allocations| allocations.set(allocations.get() + 1));
}

/// Allocations the current thread has asked for so far.
pub(crate) fn allocations() -> usize {
    ALLOCATIONS.with(core::cell::Cell::get)
}

/// Runs `body` and answers how many times it allocated.
pub(crate) fn allocations_of<T>(body: impl FnOnce() -> T) -> usize {
    let before = allocations();
    let answer = body();
    let counted = allocations() - before;
    drop(answer);
    counted
}

// SAFETY: The wrapper preserves `System` allocation contracts and adds
// per-thread accounting only.
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: The caller provides the allocation contract required by
        // `System`.
        unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        record_allocation();
        // SAFETY: The caller provides the allocation contract required by
        // `System`.
        unsafe { std::alloc::GlobalAlloc::alloc_zeroed(&std::alloc::System, layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: The caller provides the pointer and layout returned by this
        // allocator.
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        record_allocation();
        // SAFETY: The caller provides the reallocation contract required by
        // `System`.
        unsafe { std::alloc::GlobalAlloc::realloc(&std::alloc::System, ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
