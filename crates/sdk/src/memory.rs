use orca_harness_extensions::{MemoryExtension, MemoryRecord, MemoryScope, MemoryStore};

use crate::SdkError;

#[derive(Clone)]
pub struct Memory {
    store: MemoryStore,
    scope: MemoryScope,
}

impl Memory {
    pub(crate) fn new(store: MemoryStore, scope: MemoryScope) -> Self {
        Self { store, scope }
    }

    pub fn store(&self) -> &MemoryStore {
        &self.store
    }

    pub fn scope(&self) -> &MemoryScope {
        &self.scope
    }

    pub fn save(
        &self,
        content: &str,
        kind: &str,
        is_global: bool,
        source_call_id: &str,
    ) -> Result<MemoryRecord, SdkError> {
        Ok(self
            .store
            .save(&self.scope, content, kind, is_global, source_call_id)?)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryRecord>, SdkError> {
        Ok(self.store.search(&self.scope, query, limit)?)
    }

    pub fn list(&self, limit: usize) -> Result<Vec<MemoryRecord>, SdkError> {
        Ok(self.store.list(&self.scope, limit)?)
    }

    pub fn update(
        &self,
        id: &str,
        content: &str,
        kind: Option<&str>,
    ) -> Result<Option<MemoryRecord>, SdkError> {
        Ok(self.store.update(&self.scope, id, content, kind)?)
    }

    pub fn forget(&self, id: &str) -> Result<bool, SdkError> {
        Ok(self.store.forget(&self.scope, id)?)
    }
}

#[derive(Clone)]
pub struct MemoryConfig {
    pub(crate) memory: Memory,
    pub(crate) automatic_recall: bool,
    pub(crate) search_tool: bool,
    pub(crate) manage_tool: bool,
    max_items: usize,
    max_chars: usize,
}

impl MemoryConfig {
    pub fn new(memory: Memory) -> Self {
        Self {
            memory,
            automatic_recall: true,
            search_tool: true,
            manage_tool: false,
            max_items: 8,
            max_chars: 4 * 1024,
        }
    }

    pub fn read_write(memory: Memory) -> Self {
        Self::new(memory).manage_tool(true)
    }

    pub fn automatic_recall(mut self, enabled: bool) -> Self {
        self.automatic_recall = enabled;
        self
    }

    pub fn search_tool(mut self, enabled: bool) -> Self {
        self.search_tool = enabled;
        self
    }

    pub fn manage_tool(mut self, enabled: bool) -> Self {
        self.manage_tool = enabled;
        self
    }

    pub fn max_recalled_items(mut self, max_items: usize) -> Self {
        self.max_items = max_items;
        self
    }

    pub fn max_recalled_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
    }

    pub(crate) fn extension(&self) -> MemoryExtension {
        MemoryExtension::new(self.memory.store.clone(), self.memory.scope.clone())
            .max_items(self.max_items)
            .max_chars(self.max_chars)
    }
}
