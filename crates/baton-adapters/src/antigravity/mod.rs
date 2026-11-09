mod adapter;
mod desktop_adapter;
mod ide_adapter;
mod parser;
mod summary;
#[cfg(test)]
mod tests;
mod writer;

pub use adapter::AntigravityAdapter;
pub use desktop_adapter::AntigravityDesktopAdapter;
pub use ide_adapter::AntigravityIdeAdapter;
