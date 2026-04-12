mod compression;
pub mod ifd;
pub mod parser;
pub(crate) mod reader;

pub use reader::{TiffReader, TiffReaderSnapshot};
