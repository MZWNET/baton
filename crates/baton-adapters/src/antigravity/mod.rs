mod antigravity_adapter;
mod conversation;
mod desktop_adapter;
mod ide_adapter;
mod step_writer;
mod summary;
#[cfg(test)]
mod tests;

pub use antigravity_adapter::AntigravityAdapter;
pub use desktop_adapter::AntigravityDesktopAdapter;
pub use ide_adapter::AntigravityIdeAdapter;
