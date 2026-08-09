//! Application-provided random-access input and companion resolution.

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::common::error::{BioFormatsError, Result};

/// Error returned by an application source or companion resolver.
pub type SourceError = Box<dyn Error + Send + Sync + 'static>;

/// Result returned by an application source or companion resolver.
pub type SourceResult<T> = std::result::Result<T, SourceError>;

/// Stable identity for one immutable source in a resolver namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(Arc<str>);

impl SourceId {
    pub fn new(identity: impl Into<Arc<str>>) -> Self {
        Self(identity.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Immutable description of one source snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceInfo {
    identity: SourceId,
    name: Arc<str>,
    len: u64,
}

impl SourceInfo {
    pub fn new(identity: SourceId, name: impl Into<Arc<str>>, len: u64) -> Self {
        Self {
            identity,
            name: name.into(),
            len,
        }
    }

    pub fn identity(&self) -> &SourceId {
        &self.identity
    }

    /// Logical name used for format hints and companion naming.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Immutable, thread-safe, exact random-access byte source.
///
/// `read_at` must fill all of `destination` or return an error. Calls may be
/// concurrent and arrive in any order. The source identity, name, length, and
/// bytes must remain stable for as long as an opened dataset retains it.
pub trait RandomAccessSource: Send + Sync + 'static {
    fn info(&self) -> &SourceInfo;
    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()>;
}

/// A companion lookup requested by a format reader while opening a dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompanionReference<'a> {
    /// One exact reference declared by metadata.
    Named(&'a str),
    /// The complete sibling set for convention-based multi-part formats.
    Siblings,
}

impl CompanionReference<'_> {
    fn description(self) -> String {
        match self {
            Self::Named(name) => name.to_owned(),
            Self::Siblings => "<siblings>".to_owned(),
        }
    }
}

/// Resolves metadata-declared companions and complete implicit sibling sets.
pub trait CompanionResolver: Send + Sync + 'static {
    /// `Named` must return zero or one source. `Siblings` returns a complete
    /// set; readers de-duplicate and order it according to format semantics.
    fn resolve(
        &self,
        from: &SourceInfo,
        reference: CompanionReference<'_>,
    ) -> SourceResult<Vec<Arc<dyn RandomAccessSource>>>;
}

#[derive(Clone)]
enum ResolverKind {
    None,
    Application(Arc<dyn CompanionResolver>),
    Filesystem(Arc<FilesystemResolver>),
}

/// Primary source plus the namespace used to resolve its companions.
#[derive(Clone)]
pub struct SourceInput {
    primary: Arc<dyn RandomAccessSource>,
    primary_path: Option<PathBuf>,
    resolver: ResolverKind,
}

impl SourceInput {
    /// Create single-source input. Companion requests fail as missing unless a
    /// resolver is installed with `with_companion_resolver`.
    pub fn new(primary: Arc<dyn RandomAccessSource>) -> Self {
        Self {
            primary,
            primary_path: None,
            resolver: ResolverKind::None,
        }
    }

    pub fn with_companion_resolver(mut self, resolver: Arc<dyn CompanionResolver>) -> Self {
        self.resolver = ResolverKind::Application(resolver);
        self
    }

    pub fn primary(&self) -> &Arc<dyn RandomAccessSource> {
        &self.primary
    }

    pub(crate) fn from_path(path: &Path) -> Result<Self> {
        let resolver = Arc::new(FilesystemResolver);
        let source = resolver.open(path)?;
        Ok(Self {
            primary: Arc::clone(&source.source),
            primary_path: source.path,
            resolver: ResolverKind::Filesystem(resolver),
        })
    }

    pub(crate) fn primary_handle(&self) -> Result<SourceHandle> {
        SourceHandle::new(Arc::clone(&self.primary), self.primary_path.clone())
    }

    pub(crate) fn primary_path(&self) -> Option<&Path> {
        self.primary_path.as_deref()
    }

    pub(crate) fn resolve(
        &self,
        from: &SourceHandle,
        reference: CompanionReference<'_>,
    ) -> Result<Vec<SourceHandle>> {
        let resolved = match &self.resolver {
            ResolverKind::None => return Ok(Vec::new()),
            ResolverKind::Application(resolver) => resolver
                .resolve(from.info(), reference)
                .map_err(|source| BioFormatsError::CompanionResolution {
                    identity: from.info().identity().clone(),
                    reference: reference.description(),
                    source,
                })?
                .into_iter()
                .map(|source| SourceHandle::new(source, None))
                .collect::<Result<Vec<_>>>()?,
            ResolverKind::Filesystem(resolver) => resolver.resolve(from, reference)?,
        };

        if matches!(reference, CompanionReference::Named(_)) && resolved.len() > 1 {
            return Err(BioFormatsError::CompanionAmbiguous {
                identity: from.info().identity().clone(),
                reference: reference.description(),
                count: resolved.len(),
            });
        }
        Ok(resolved)
    }

    /// Resolve an implicit sibling set while allowing a reader to apply its
    /// naming convention before the filesystem adapter opens candidates.
    pub(crate) fn resolve_siblings_where(
        &self,
        from: &SourceHandle,
        include: impl Fn(&str) -> bool,
    ) -> Result<Vec<SourceHandle>> {
        match &self.resolver {
            ResolverKind::None => Ok(Vec::new()),
            ResolverKind::Application(_) => {
                let mut sources = self.resolve(from, CompanionReference::Siblings)?;
                sources.retain(|source| include(source.info().name()));
                Ok(sources)
            }
            ResolverKind::Filesystem(resolver) => resolver.resolve_siblings_where(from, include),
        }
    }
}

/// Checked internal source handle shared by all built-in readers.
#[derive(Clone)]
pub(crate) struct SourceHandle {
    source: Arc<dyn RandomAccessSource>,
    info: SourceInfo,
    path: Option<PathBuf>,
}

impl fmt::Debug for SourceHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceHandle")
            .field("info", &self.info)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SourceHandle {
    fn new(source: Arc<dyn RandomAccessSource>, path: Option<PathBuf>) -> Result<Self> {
        let info = source.info().clone();
        if info.identity().as_str().is_empty() {
            return Err(BioFormatsError::InvalidData(
                "source identity must not be empty".into(),
            ));
        }
        if info.name().is_empty() {
            return Err(BioFormatsError::InvalidData(
                "source logical name must not be empty".into(),
            ));
        }
        Ok(Self { source, info, path })
    }

    pub(crate) fn info(&self) -> &SourceInfo {
        &self.info
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<()> {
        if self.source.info() != &self.info {
            return Err(BioFormatsError::SourceChanged {
                identity: self.info.identity().clone(),
            });
        }
        let length = u64::try_from(destination.len())
            .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        let end =
            offset
                .checked_add(length)
                .ok_or_else(|| BioFormatsError::SourceRangeOverflow {
                    identity: self.info.identity().clone(),
                    offset,
                    length,
                })?;
        if end > self.info.len() {
            return Err(BioFormatsError::SourceRangeOutOfBounds {
                identity: self.info.identity().clone(),
                offset,
                length,
                source_len: self.info.len(),
            });
        }
        self.source
            .read_at(offset, destination)
            .map_err(|source| BioFormatsError::SourceRead {
                identity: self.info.identity().clone(),
                offset,
                length,
                source,
            })
    }

    pub(crate) fn read_prefix(&self, maximum: usize) -> Result<Vec<u8>> {
        let available = usize::try_from(self.info.len()).unwrap_or(usize::MAX);
        let length = maximum.min(available);
        if length > isize::MAX as usize {
            return Err(BioFormatsError::PlaneByteCountOverflow);
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|error| {
            BioFormatsError::InvalidData(format!(
                "cannot allocate {length}-byte source header: {error}"
            ))
        })?;
        bytes.resize(length, 0);
        self.read_at(0, &mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn read_all(&self, context: &str) -> Result<Vec<u8>> {
        let length = usize::try_from(self.info.len())
            .map_err(|_| BioFormatsError::PlaneByteCountOverflow)?;
        if length > isize::MAX as usize {
            return Err(BioFormatsError::InvalidData(format!(
                "{context} does not fit in memory"
            )));
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|error| {
            BioFormatsError::InvalidData(format!(
                "cannot allocate {length}-byte {context}: {error}"
            ))
        })?;
        bytes.resize(length, 0);
        self.read_at(0, &mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn cursor(&self) -> SourceCursor {
        SourceCursor {
            source: self.clone(),
            position: 0,
        }
    }
}

/// Independent `Read + Seek` cursor over a checked random-access source.
#[derive(Clone, Debug)]
pub(crate) struct SourceCursor {
    source: SourceHandle,
    position: u64,
}

impl Read for SourceCursor {
    fn read(&mut self, destination: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.source.info().len().saturating_sub(self.position);
        let length = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(destination.len());
        if length == 0 {
            return Ok(0);
        }
        self.source
            .read_at(self.position, &mut destination[..length])
            .map_err(std::io::Error::other)?;
        self.position = self
            .position
            .checked_add(length as u64)
            .ok_or_else(|| std::io::Error::other("source cursor position overflow"))?;
        Ok(length)
    }
}

impl Seek for SourceCursor {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => position,
            SeekFrom::End(delta) => checked_signed_offset(self.source.info().len(), delta)?,
            SeekFrom::Current(delta) => checked_signed_offset(self.position, delta)?,
        };
        self.position = next;
        Ok(next)
    }
}

fn checked_signed_offset(base: u64, delta: i64) -> std::io::Result<u64> {
    if delta >= 0 {
        base.checked_add(delta as u64)
    } else {
        base.checked_sub(delta.unsigned_abs())
    }
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid source seek"))
}

/// Recover a structured source error carried through a `Read + Seek` adapter.
pub(crate) fn map_source_io_error(error: std::io::Error) -> BioFormatsError {
    if error
        .get_ref()
        .is_some_and(|inner| inner.is::<BioFormatsError>())
    {
        let inner = error
            .into_inner()
            .expect("checked source cursor error has an inner error")
            .downcast::<BioFormatsError>()
            .expect("checked source cursor error has the expected type");
        normalize_source_error(*inner)
    } else {
        BioFormatsError::Io(error)
    }
}

pub(crate) fn normalize_source_error(error: BioFormatsError) -> BioFormatsError {
    match error {
        BioFormatsError::Io(error) => map_source_io_error(error),
        error => error,
    }
}

struct FilesystemSource {
    info: SourceInfo,
    file: Mutex<File>,
}

impl RandomAccessSource for FilesystemSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> SourceResult<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("filesystem source lock poisoned"))?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(destination)?;
        Ok(())
    }
}

struct FilesystemResolver;

impl FilesystemResolver {
    fn open(&self, path: &Path) -> Result<SourceHandle> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let identity_path = std::fs::canonicalize(path)?;
        let logical_name: Arc<str> = path.to_string_lossy().into_owned().into();
        let identity = SourceId::new(format!("file:{}", identity_path.to_string_lossy()));
        let source: Arc<dyn RandomAccessSource> = Arc::new(FilesystemSource {
            info: SourceInfo::new(identity, logical_name, len),
            file: Mutex::new(file),
        });
        SourceHandle::new(source, Some(path.to_path_buf()))
    }

    fn resolve(
        &self,
        from: &SourceHandle,
        reference: CompanionReference<'_>,
    ) -> Result<Vec<SourceHandle>> {
        let from_path = from.path().ok_or_else(|| {
            BioFormatsError::InvalidData("filesystem source lost its path context".into())
        })?;
        match reference {
            CompanionReference::Named(name) => {
                let requested = Path::new(name);
                let path = if requested.is_absolute() {
                    requested.to_path_buf()
                } else {
                    from_path
                        .parent()
                        .unwrap_or_else(|| Path::new(""))
                        .join(requested)
                };
                match self.open(&path) {
                    Ok(source) => Ok(vec![source]),
                    Err(BioFormatsError::Io(error))
                        if error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        Ok(Vec::new())
                    }
                    Err(error) => Err(error),
                }
            }
            CompanionReference::Siblings => self.resolve_siblings_where(from, |_| true),
        }
    }

    fn resolve_siblings_where(
        &self,
        from: &SourceHandle,
        include: impl Fn(&str) -> bool,
    ) -> Result<Vec<SourceHandle>> {
        let from_path = from.path().ok_or_else(|| {
            BioFormatsError::InvalidData("filesystem source lost its path context".into())
        })?;
        let parent = from_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut sources = Vec::new();
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let path = entry.path();
            if include(&path.to_string_lossy()) && entry.file_type()?.is_file() {
                sources.push(self.open(&path)?);
            }
        }
        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn filesystem_identity_is_stable_across_lexical_path_aliases() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "bioformats-rs-source-identity-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create source identity directory");
        let direct_path = directory.join("sample.czi");
        std::fs::write(&direct_path, b"source").expect("write source identity fixture");
        let aliased_path = directory.join(".").join("sample.czi");

        let resolver = FilesystemResolver;
        let direct = resolver.open(&direct_path).expect("open direct path");
        let aliased = resolver.open(&aliased_path).expect("open aliased path");
        assert_eq!(direct.info().identity(), aliased.info().identity());

        std::fs::remove_file(&direct_path).expect("remove source identity fixture");
        std::fs::remove_dir(&directory).expect("remove source identity directory");
    }

    #[test]
    fn filesystem_siblings_are_filtered_before_sources_are_opened() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "bioformats-rs-source-siblings-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create source sibling directory");
        let master = directory.join("sample.czi");
        let part = directory.join("sample (1).czi");
        let unrelated = directory.join("other.czi");
        for path in [&master, &part, &unrelated] {
            std::fs::write(path, b"source").expect("write source sibling fixture");
        }

        let input = SourceInput::from_path(&master).expect("create filesystem input");
        let primary = input.primary_handle().expect("create primary handle");
        let siblings = input
            .resolve_siblings_where(&primary, |name| {
                Path::new(name)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem == "sample" || stem.starts_with("sample ("))
            })
            .expect("resolve filtered siblings");
        assert_eq!(siblings.len(), 2);
        assert!(siblings
            .iter()
            .all(|source| source.info().name().contains("sample")));

        for path in [&master, &part, &unrelated] {
            std::fs::remove_file(path).expect("remove source sibling fixture");
        }
        std::fs::remove_dir(&directory).expect("remove source sibling directory");
    }
}
