pub(super) fn pthread_to_u64(t: libc::pthread_t) -> u64 {
    let mut out: u64 = 0;
    let n = std::cmp::min(
        std::mem::size_of::<libc::pthread_t>(),
        std::mem::size_of::<u64>(),
    );
    unsafe {
        std::ptr::copy_nonoverlapping(
            &t as *const _ as *const u8,
            &mut out as *mut _ as *mut u8,
            n,
        );
    }
    out
}

pub(super) fn u64_to_pthread(id: u64) -> libc::pthread_t {
    let mut t: libc::pthread_t = unsafe { std::mem::zeroed() };
    let n = std::cmp::min(
        std::mem::size_of::<libc::pthread_t>(),
        std::mem::size_of::<u64>(),
    );
    unsafe {
        std::ptr::copy_nonoverlapping(&id as *const _ as *const u8, &mut t as *mut _ as *mut u8, n);
    }
    t
}

pub(crate) fn current_thread_id() -> u64 {
    pthread_to_u64(unsafe { libc::pthread_self() })
}

pub(super) fn threads_equal(a: u64, b: u64) -> bool {
    if a == 0 || b == 0 {
        return false;
    }
    unsafe { libc::pthread_equal(u64_to_pthread(a), u64_to_pthread(b)) != 0 }
}

pub(super) fn sleep_ms(ms: i32) {
    if ms <= 0 {
        return;
    }
    unsafe {
        libc::usleep((ms as u32).saturating_mul(1000));
    }
}
