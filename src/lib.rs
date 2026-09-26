//! Arena-backed segmented byte streams using standard IO traits.
//!
//! [`Writer`] implements [`std::io::Write`]. Convert it into a [`Reader`]
//! without copying payloads; the reader implements [`std::io::Read`] and
//! [`std::io::BufRead`]. The default-enabled `tokio` feature implements the
//! corresponding asynchronous traits on these same types. Disable default
//! features for synchronous-only use.
//!
//! ```
//! use brz_ds::EphemeralBytesArena;
//! use brz_io::{Reader, Writer};
//! use std::io::{Read, Write};
//!
//! let arena = EphemeralBytesArena::new(64 * 1024);
//! let mut writer = Writer::new(&arena);
//! writer.write_all(b"hello ")?;
//! writer.write_all(b"world")?;
//! let mut reader: Reader = writer.into_reader();
//! let mut text = String::new();
//! reader.read_to_string(&mut text)?;
//! assert_eq!(text, "hello world");
//! # Ok::<(), std::io::Error>(())
//! ```

mod reader;
mod segments;
mod writer;

#[cfg(feature = "tokio")]
mod asynchronous;

pub use reader::{Reader, ReaderView};
pub use writer::Writer;

pub use brz_ds::EphemeralBytesArena;
