//! The threaded event loop: reads the tty, parses input, and pushes events
//! onto the [`crate::queue`].

// NOTE: upstream names this module `loop`. That is a reserved keyword in Rust,
// so we name the module `event_loop` and keep the struct as `Loop`.

/// Threaded reader loop driving the input parser and the event queue.
pub struct Loop;
