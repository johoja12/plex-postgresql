//! Retire thread ownership without releasing a connection still held by a handle
//! or statement. The acquire-side maintenance pass makes the safe release.
use super::threading::{current_thread_id, threads_equal};
use super::{PoolManager, POOL, SLOT_READY};
use std::sync::atomic::Ordering;

struct OwnerExit(u64);
impl Drop for OwnerExit {
    fn drop(&mut self) {
        if let Some(pm) = POOL.get() {
            retire_owned_slots(pm, self.0);
        }
    }
}

thread_local! {
    static OWNER_EXIT: OwnerExit = OwnerExit(current_thread_id());
}

pub(super) fn register_owner_exit() {
    let _ = OWNER_EXIT.try_with(|_| {});
}

pub(super) fn retire_owned_slots(pm: &PoolManager, owner: u64) {
    for slot in &pm.slots[..pm.pool_size()] {
        let generation = slot.generation.load(Ordering::Acquire);
        if slot.state.load(Ordering::Acquire) == SLOT_READY
            && threads_equal(slot.owner_thread.load(Ordering::Acquire), owner)
        {
            slot.retired_generation
                .store(u64::from(generation), Ordering::Release);
        }
    }
}
