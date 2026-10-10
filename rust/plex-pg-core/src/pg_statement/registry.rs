use std::collections::HashMap;

pub(super) struct StmtRegistry {
    pub(super) forward: HashMap<usize, usize>,
    reverse: HashMap<usize, usize>,
}

impl StmtRegistry {
    pub(super) fn new() -> Self {
        Self {
            forward: HashMap::with_capacity(512),
            reverse: HashMap::with_capacity(512),
        }
    }

    pub(super) fn register(&mut self, sqlite_stmt: usize, pg_stmt: usize) {
        if let Some(old_pg) = self.forward.insert(sqlite_stmt, pg_stmt) {
            if old_pg != pg_stmt {
                self.reverse.remove(&old_pg);
            }
        }
        self.reverse.insert(pg_stmt, sqlite_stmt);
        super::registry_api::invalidate_fast_lookup();
    }

    pub(super) fn unregister(&mut self, sqlite_stmt: usize) {
        if let Some(pg_stmt) = self.forward.remove(&sqlite_stmt) {
            self.reverse.remove(&pg_stmt);
            super::registry_api::invalidate_fast_lookup();
        }
    }

    pub(super) fn find(&self, sqlite_stmt: usize) -> Option<usize> {
        self.forward.get(&sqlite_stmt).copied()
    }

    pub(super) fn is_ours(&self, pg_stmt: usize) -> bool {
        self.reverse.contains_key(&pg_stmt)
    }

    pub(super) fn clear(&mut self) {
        self.forward.clear();
        self.reverse.clear();
        super::registry_api::invalidate_fast_lookup();
    }

    pub(super) fn len(&self) -> usize {
        self.forward.len()
    }
}
