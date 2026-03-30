# Rust Development Guidelines

- Prefer `anyhow::Result` for application-level error handling
- Use `thiserror` for library-level custom errors
- Always handle `Result` and `Option` — avoid `.unwrap()` in production code
- Use `tokio` for async runtime, prefer `tokio::spawn` for concurrent tasks
- Prefer `Arc<T>` for shared ownership across threads, `Arc<RwLock<T>>` for mutable shared state
- Use `tracing` for structured logging (`info!`, `warn!`, `error!`)
- Run `cargo clippy` before committing
- Keep functions small and focused — extract helpers when logic exceeds ~30 lines
- Use `#[derive(Debug, Clone)]` on structs that need it
- Prefer `&str` over `String` in function parameters when ownership isn't needed
