//! Reading native sessions stored as JSON Lines, in full or incrementally after a [`Cursor`](crate::Cursor).
//!
//! An adapter turns each line into a [`Line`] through its [`Parser`]; [`read`] then follows the active branch of the resulting tree, or continues the branch recorded in the cursor when the file only grew since.

mod parser;
mod reader;
mod tree;

pub use parser::{Line, Node, Parser};
pub use reader::{JsonLine, is_compressed, lines, read};
