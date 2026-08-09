//! Adapting an application's range-capable object store to bioformats-rs.

use std::collections::HashMap;
use std::sync::Arc;

use bioformats_rs::{
    open_source, CompanionReference, CompanionResolver, Dataset, RandomAccessSource, SourceId,
    SourceInfo, SourceInput, SourceResult,
};

/// The host application's existing range-read boundary (object store, archive,
/// database blob, HTTP range client, and so on).
pub trait RangeStore: Send + Sync + 'static {
    fn read_exact_range(&self, key: &str, offset: u64, destination: &mut [u8]) -> SourceResult<()>;
}

pub struct StoreSource {
    store: Arc<dyn RangeStore>,
    key: Arc<str>,
    info: SourceInfo,
}

impl StoreSource {
    pub fn new(
        store: Arc<dyn RangeStore>,
        key: impl Into<Arc<str>>,
        name: impl Into<Arc<str>>,
        len: u64,
    ) -> Self {
        let key = key.into();
        let info = SourceInfo::new(SourceId::new(format!("asset:{key}")), name, len);
        Self { store, key, info }
    }
}

impl RandomAccessSource for StoreSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        self.store.read_exact_range(&self.key, offset, destination)
    }
}

/// Applications decide how logical companion names map into their own asset
/// namespace. The sibling list must be complete for split-file formats.
pub struct StoreResolver {
    pub named: HashMap<String, Arc<dyn RandomAccessSource>>,
    pub siblings: Vec<Arc<dyn RandomAccessSource>>,
}

impl CompanionResolver for StoreResolver {
    fn resolve(
        &self,
        _from: &SourceInfo,
        reference: CompanionReference<'_>,
    ) -> SourceResult<Vec<Arc<dyn RandomAccessSource>>> {
        Ok(match reference {
            CompanionReference::Named(name) => self.named.get(name).cloned().into_iter().collect(),
            CompanionReference::Siblings => self.siblings.clone(),
            _ => Vec::new(),
        })
    }
}

pub fn open_application_asset(
    primary: Arc<dyn RandomAccessSource>,
    resolver: Arc<dyn CompanionResolver>,
) -> bioformats_rs::Result<Dataset> {
    open_source(SourceInput::new(primary).with_companion_resolver(resolver))
}

fn main() {}
