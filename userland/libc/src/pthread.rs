// POSIX threads — implemented on top of toyos-abi thread/futex syscalls.

use alloc::alloc::{alloc as heap_alloc, dealloc as heap_dealloc};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use toyos_abi::syscall;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// pthread_t is a thread ID (u64 from kernel)
type PthreadT = u64;

// Mutex: futex-based. 0 = unlocked, 1 = locked, 2 = locked with waiters.
#[repr(C)]
pub struct PthreadMutexT {
    state: AtomicU32,
}

// Condition variable: futex-based.
#[repr(C)]
pub struct PthreadCondT {
    seq: AtomicU32,
}

// Mutex/cond attr (unused, but needed for API compat)
type PthreadMutexattrT = u64;
type PthreadCondattrT = u64;

// Once control
#[repr(C)]
pub struct PthreadOnceT {
    state: AtomicU32, // 0 = not called, 1 = in progress, 2 = done
}

// Thread-local storage
type PthreadKeyT = u32;

const PTHREAD_KEYS_MAX: usize = 128;
static mut KEY_DESTRUCTORS: [Option<unsafe extern "C" fn(*mut u8)>; PTHREAD_KEYS_MAX] =
    [None; PTHREAD_KEYS_MAX];
static NEXT_KEY: AtomicU32 = AtomicU32::new(0);

// Per-thread TLS values — stored in a global array indexed by thread ID.
// This is a simplification; real implementations use TLS segments.
const MAX_THREADS: usize = 64;
static mut TLS_VALUES: [[*mut u8; PTHREAD_KEYS_MAX]; MAX_THREADS] =
    [[ptr::null_mut(); PTHREAD_KEYS_MAX]; MAX_THREADS];

fn thread_index() -> usize {
    // Use thread ID mod MAX_THREADS as index
    let tid = syscall::getpid().0 as usize; // approximate — uses pid, not tid
    tid % MAX_THREADS
}

// ---------------------------------------------------------------------------
// Thread create/join
// ---------------------------------------------------------------------------

const THREAD_STACK_SIZE: usize = 1024 * 1024; // 1 MiB

// Trampoline for pthread_create: calls the user function, then exits the thread.
unsafe extern "C" fn thread_entry(arg: u64) {
    let info = arg as *mut ThreadStartInfo;
    let start_routine = (*info).start_routine;
    let user_arg = (*info).arg;
    // Free the info struct
    let layout = core::alloc::Layout::new::<ThreadStartInfo>();
    heap_dealloc(info as *mut u8, layout);
    // Call user function
    let _retval = start_routine(user_arg);
    syscall::thread_exit(0);
}

struct ThreadStartInfo {
    start_routine: unsafe extern "C" fn(*mut u8) -> *mut u8,
    arg: *mut u8,
}

#[no_mangle]
pub unsafe extern "C" fn pthread_create(
    thread: *mut PthreadT,
    _attr: *const u8,
    start_routine: unsafe extern "C" fn(*mut u8) -> *mut u8,
    arg: *mut u8,
) -> i32 {
    // Allocate thread start info on heap (freed by trampoline)
    let layout = core::alloc::Layout::new::<ThreadStartInfo>();
    let info = heap_alloc(layout) as *mut ThreadStartInfo;
    if info.is_null() { return -1; }
    ptr::write(info, ThreadStartInfo { start_routine, arg });

    // Allocate stack
    let stack_layout = core::alloc::Layout::from_size_align(THREAD_STACK_SIZE, 16).unwrap();
    let stack_base = heap_alloc(stack_layout);
    if stack_base.is_null() {
        heap_dealloc(info as *mut u8, layout);
        return -1;
    }
    let stack_top = stack_base.add(THREAD_STACK_SIZE);
    // Align stack to 16 bytes
    let stack_ptr = ((stack_top as usize) & !0xF) as u64;

    // SAFETY: entry point, stack, and argument are valid; stack is freshly allocated and aligned
    let tid = unsafe { syscall::thread_spawn(
        thread_entry as *const () as u64,
        stack_ptr,
        info as u64,
        stack_base as u64,
    ) };

    if !thread.is_null() {
        *thread = tid;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_join(thread: PthreadT, retval: *mut *mut u8) -> i32 {
    syscall::thread_join(thread);
    if !retval.is_null() {
        *retval = ptr::null_mut();
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_detach(_thread: PthreadT) -> i32 {
    0 // ToyOS threads are implicitly cleaned up
}

#[no_mangle]
pub unsafe extern "C" fn pthread_self() -> PthreadT {
    // Return pid as a stand-in for thread ID
    syscall::getpid().0 as u64
}

#[no_mangle]
pub unsafe extern "C" fn pthread_equal(t1: PthreadT, t2: PthreadT) -> i32 {
    (t1 == t2) as i32
}

// ---------------------------------------------------------------------------
// Mutex (futex-based)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_init(
    mutex: *mut PthreadMutexT, _attr: *const PthreadMutexattrT,
) -> i32 {
    (*mutex).state = AtomicU32::new(0);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_lock(mutex: *mut PthreadMutexT) -> i32 {
    let state = &(*mutex).state;
    // Fast path: try to acquire (0 -> 1)
    if state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        return 0;
    }
    // Slow path: set to 2 (locked + waiters) and wait
    loop {
        // If not already 2, set it to mark waiters
        let old = state.swap(2, Ordering::Acquire);
        if old == 0 {
            return 0; // Got the lock
        }
        // Wait until state changes from 2
        let addr = state as *const AtomicU32 as *const u32;
        // SAFETY: addr points to valid atomic state owned by the mutex
        unsafe { syscall::futex_wait(addr, 2, None) };
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_trylock(mutex: *mut PthreadMutexT) -> i32 {
    let state = &(*mutex).state;
    if state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        0
    } else {
        16 // EBUSY
    }
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut PthreadMutexT) -> i32 {
    let state = &(*mutex).state;
    let old = state.swap(0, Ordering::Release);
    if old == 2 {
        // There were waiters, wake one
        let addr = state as *const AtomicU32 as *const u32;
        // SAFETY: addr points to valid atomic state owned by the mutex
        unsafe { syscall::futex_wake(addr, 1) };
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutex_destroy(_mutex: *mut PthreadMutexT) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_init(_attr: *mut PthreadMutexattrT) -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_destroy(_attr: *mut PthreadMutexattrT) -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn pthread_mutexattr_settype(
    _attr: *mut PthreadMutexattrT, _type: i32,
) -> i32 { 0 }

// ---------------------------------------------------------------------------
// Condition variable (futex-based)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_init(
    cond: *mut PthreadCondT, _attr: *const PthreadCondattrT,
) -> i32 {
    (*cond).seq = AtomicU32::new(0);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_wait(
    cond: *mut PthreadCondT, mutex: *mut PthreadMutexT,
) -> i32 {
    let seq = (*cond).seq.load(Ordering::Relaxed);
    pthread_mutex_unlock(mutex);
    let addr = &(*cond).seq as *const AtomicU32 as *const u32;
    // SAFETY: addr points to valid atomic state owned by the condvar
    unsafe { syscall::futex_wait(addr, seq, None) };
    pthread_mutex_lock(mutex);
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut PthreadCondT) -> i32 {
    (*cond).seq.fetch_add(1, Ordering::Release);
    let addr = &(*cond).seq as *const AtomicU32 as *const u32;
    // SAFETY: addr points to valid atomic state owned by the condvar
    unsafe { syscall::futex_wake(addr, 1) };
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut PthreadCondT) -> i32 {
    (*cond).seq.fetch_add(1, Ordering::Release);
    let addr = &(*cond).seq as *const AtomicU32 as *const u32;
    // SAFETY: addr points to valid atomic state owned by the condvar
    unsafe { syscall::futex_wake(addr, u32::MAX) };
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_cond_destroy(_cond: *mut PthreadCondT) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_condattr_init(_attr: *mut PthreadCondattrT) -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn pthread_condattr_destroy(_attr: *mut PthreadCondattrT) -> i32 { 0 }

// ---------------------------------------------------------------------------
// Once
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_once(
    once: *mut PthreadOnceT, init_routine: unsafe extern "C" fn(),
) -> i32 {
    let state = &(*once).state;
    // Already done?
    if state.load(Ordering::Acquire) == 2 {
        return 0;
    }
    // Try to be the one to run it (0 -> 1)
    if state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Acquire).is_ok() {
        init_routine();
        state.store(2, Ordering::Release);
        // Wake any waiters
        let addr = state as *const AtomicU32 as *const u32;
        // SAFETY: addr points to valid atomic state owned by the once control
        unsafe { syscall::futex_wake(addr, u32::MAX) };
        return 0;
    }
    // Someone else is running it, wait
    loop {
        let addr = state as *const AtomicU32 as *const u32;
        // SAFETY: addr points to valid atomic state owned by the once control
        unsafe { syscall::futex_wait(addr, 1, None) };
        if state.load(Ordering::Acquire) == 2 {
            return 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local storage (TLS keys)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_key_create(
    key: *mut PthreadKeyT, destructor: Option<unsafe extern "C" fn(*mut u8)>,
) -> i32 {
    let k = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
    if k as usize >= PTHREAD_KEYS_MAX {
        return -1; // EAGAIN
    }
    KEY_DESTRUCTORS[k as usize] = destructor;
    *key = k;
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_key_delete(_key: PthreadKeyT) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_getspecific(key: PthreadKeyT) -> *mut u8 {
    if key as usize >= PTHREAD_KEYS_MAX { return ptr::null_mut(); }
    TLS_VALUES[thread_index()][key as usize]
}

#[no_mangle]
pub unsafe extern "C" fn pthread_setspecific(key: PthreadKeyT, value: *const u8) -> i32 {
    if key as usize >= PTHREAD_KEYS_MAX { return -1; }
    TLS_VALUES[thread_index()][key as usize] = value as *mut u8;
    0
}

// ---------------------------------------------------------------------------
// RWLock (simple: wraps mutex, no reader parallelism)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct PthreadRwlockT {
    mutex: PthreadMutexT,
}

#[no_mangle]
pub unsafe extern "C" fn pthread_rwlock_init(
    rwlock: *mut PthreadRwlockT, _attr: *const u8,
) -> i32 {
    pthread_mutex_init(&mut (*rwlock).mutex, ptr::null())
}

#[no_mangle]
pub unsafe extern "C" fn pthread_rwlock_rdlock(rwlock: *mut PthreadRwlockT) -> i32 {
    pthread_mutex_lock(&mut (*rwlock).mutex)
}

#[no_mangle]
pub unsafe extern "C" fn pthread_rwlock_wrlock(rwlock: *mut PthreadRwlockT) -> i32 {
    pthread_mutex_lock(&mut (*rwlock).mutex)
}

#[no_mangle]
pub unsafe extern "C" fn pthread_rwlock_unlock(rwlock: *mut PthreadRwlockT) -> i32 {
    pthread_mutex_unlock(&mut (*rwlock).mutex)
}

#[no_mangle]
pub unsafe extern "C" fn pthread_rwlock_destroy(rwlock: *mut PthreadRwlockT) -> i32 {
    pthread_mutex_destroy(&mut (*rwlock).mutex)
}

// ---------------------------------------------------------------------------
// Attr stubs
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn pthread_attr_init(_attr: *mut u8) -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn pthread_attr_destroy(_attr: *mut u8) -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn pthread_attr_setstacksize(_attr: *mut u8, _size: usize) -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn pthread_attr_getstacksize(_attr: *const u8, size: *mut usize) -> i32 {
    if !size.is_null() { *size = THREAD_STACK_SIZE; }
    0
}

#[no_mangle]
pub unsafe extern "C" fn pthread_attr_setdetachstate(_attr: *mut u8, _state: i32) -> i32 { 0 }
