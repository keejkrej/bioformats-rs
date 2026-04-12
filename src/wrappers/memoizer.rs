use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::registry::ImageReader;
use crate::snapshot::{capture_fingerprint, fingerprint_matches, MemoFilePayload};

const MEMO_VERSION: u32 = 1;
const DEFAULT_MINIMUM_ELAPSED_MS: u64 = 100;

/// Caches initialized reader state to a `.bfmemo` file.
pub struct Memoizer {
    reader: Box<dyn FormatReader>,
    minimum_elapsed: Duration,
    memo_dir: Option<PathBuf>,
    loaded_from_memo: bool,
    saved_to_memo: bool,
    current_memo_file: Option<PathBuf>,
}

impl Memoizer {
    pub fn new() -> Self {
        Self::with_box(
            Box::new(ImageReader::new()),
            Duration::from_millis(DEFAULT_MINIMUM_ELAPSED_MS),
            None,
        )
    }

    pub fn with_reader<R: FormatReader + 'static>(reader: R) -> Self {
        Self::with_box(
            Box::new(reader),
            Duration::from_millis(DEFAULT_MINIMUM_ELAPSED_MS),
            None,
        )
    }

    pub fn with_minimum_elapsed(milliseconds: u64) -> Self {
        Self::with_box(
            Box::new(ImageReader::new()),
            Duration::from_millis(milliseconds),
            None,
        )
    }

    pub fn with_config<R: FormatReader + 'static>(
        reader: R,
        minimum_elapsed: Duration,
        memo_dir: Option<PathBuf>,
    ) -> Self {
        Self::with_box(Box::new(reader), minimum_elapsed, memo_dir)
    }

    pub fn with_box(
        reader: Box<dyn FormatReader>,
        minimum_elapsed: Duration,
        memo_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            reader,
            minimum_elapsed,
            memo_dir,
            loaded_from_memo: false,
            saved_to_memo: false,
            current_memo_file: None,
        }
    }

    pub fn is_loaded_from_memo(&self) -> bool {
        self.loaded_from_memo
    }

    pub fn is_saved_to_memo(&self) -> bool {
        self.saved_to_memo
    }

    pub fn memo_file(&self) -> Option<&Path> {
        self.current_memo_file.as_deref()
    }

    pub fn get_memo_file(&self, path: &Path) -> Option<PathBuf> {
        let file_name = path.file_name()?.to_str()?;
        let memo_name = format!(".{}.bfmemo", file_name);

        if let Some(base_dir) = self.memo_dir.as_ref() {
            if !base_dir.exists() || !base_dir.is_dir() {
                return None;
            }
            let parent = path.parent()?;
            let relative_parent = parent
                .strip_prefix(Path::new("/"))
                .unwrap_or(parent)
                .to_path_buf();
            Some(base_dir.join(relative_parent).join(memo_name))
        } else {
            let parent = path.parent()?;
            if !parent.exists() {
                return None;
            }
            Some(parent.join(memo_name))
        }
    }

    pub fn delete_memo(&mut self) -> Result<()> {
        if let Some(path) = self.current_memo_file.as_ref() {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    fn load_memo(
        &self,
        source_path: &Path,
        memo_path: &Path,
    ) -> Result<Option<Box<dyn FormatReader>>> {
        if !memo_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(memo_path)?;
        let mut payload: MemoFilePayload = bincode::deserialize(&bytes)
            .map_err(|err| BioFormatsError::InvalidData(err.to_string()))?;
        if payload.version != MEMO_VERSION || !fingerprint_matches(source_path, &payload.source) {
            return Ok(None);
        }
        payload.snapshot.retarget_path(source_path);
        Ok(Some(payload.snapshot.into_reader()?))
    }

    fn save_memo(&self, source_path: &Path, memo_path: &Path) -> Result<()> {
        let snapshot = self.reader.snapshot()?;
        let payload = MemoFilePayload {
            version: MEMO_VERSION,
            source: capture_fingerprint(source_path)?,
            snapshot,
        };
        let bytes = bincode::serialize(&payload)
            .map_err(|err| BioFormatsError::InvalidData(err.to_string()))?;
        if let Some(parent) = memo_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp_path = memo_path.with_extension("bfmemo.tmp");
        fs::write(&temp_path, bytes)?;
        fs::rename(temp_path, memo_path)?;
        Ok(())
    }
}

impl Default for Memoizer {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Memoizer {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        self.reader.is_this_type_by_name(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.reader.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.loaded_from_memo = false;
        self.saved_to_memo = false;
        self.current_memo_file = self.get_memo_file(path);

        if let Some(memo_path) = self.current_memo_file.as_ref() {
            if let Some(reader) = self.load_memo(path, memo_path)? {
                self.reader = reader;
                self.loaded_from_memo = true;
                return Ok(());
            }
        }

        let start = Instant::now();
        self.reader.set_id(path)?;
        let elapsed = start.elapsed();
        if elapsed >= self.minimum_elapsed {
            if let Some(memo_path) = self.current_memo_file.as_ref() {
                self.save_memo(path, memo_path)?;
                self.saved_to_memo = true;
            }
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.reader.close()
    }

    fn series_count(&self) -> usize {
        self.reader.series_count()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        self.reader.set_series(series)
    }

    fn series(&self) -> usize {
        self.reader.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.reader.metadata()
    }

    fn current_file(&self) -> Option<&Path> {
        self.reader.current_file()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.reader.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.reader.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.reader.open_thumb_bytes(plane_index)
    }

    fn snapshot(&self) -> Result<crate::snapshot::ReaderSnapshot> {
        self.reader.snapshot()
    }

    fn resolution_count(&self) -> usize {
        self.reader.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.reader.set_resolution(level)
    }

    fn resolution(&self) -> usize {
        self.reader.resolution()
    }
}
