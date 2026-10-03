use std::cell::Cell;

use crate::ffi_types::sqlite3_stmt;

thread_local! {
    static STEPPING_SHADOW: Cell<*mut sqlite3_stmt> = const { Cell::new(std::ptr::null_mut()) };
}

/// Keep SQLite's automatic reset inside its own VM. It must not clear the PG
/// result or acquire its statement mutex when called back from original step.
pub(crate) fn with_shadow_step<T>(stmt: *mut sqlite3_stmt, step: impl FnOnce() -> T) -> T {
    struct Restore(*mut sqlite3_stmt);
    impl Drop for Restore {
        fn drop(&mut self) {
            STEPPING_SHADOW.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(STEPPING_SHADOW.with(|slot| slot.replace(stmt)));
    step()
}

pub(crate) fn is_shadow_step(stmt: *mut sqlite3_stmt) -> bool {
    !stmt.is_null() && STEPPING_SHADOW.with(|slot| slot.get() == stmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_matches_only_the_current_statement_and_restores_nested_steps() {
        let a = 1usize as *mut sqlite3_stmt;
        let b = 2usize as *mut sqlite3_stmt;
        assert!(!is_shadow_step(a));
        assert!(!is_shadow_step(std::ptr::null_mut()));
        with_shadow_step(a, || {
            assert!(is_shadow_step(a));
            assert!(!is_shadow_step(b));
            with_shadow_step(b, || {
                assert!(is_shadow_step(b));
                assert!(!is_shadow_step(a));
            });
            assert!(is_shadow_step(a));
        });
        assert!(!is_shadow_step(a));
    }

    #[test]
    fn guard_restores_on_unwind_and_does_not_cross_threads() {
        let stmt = 1usize as *mut sqlite3_stmt;
        let _ = std::panic::catch_unwind(|| with_shadow_step(stmt, || panic!("unwind")));
        assert!(!is_shadow_step(stmt));
        with_shadow_step(stmt, || {
            std::thread::spawn(|| assert!(!is_shadow_step(1usize as *mut sqlite3_stmt)))
                .join().unwrap();
            assert!(is_shadow_step(stmt));
        });
    }
}
