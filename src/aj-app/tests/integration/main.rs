//! Application contracts with fixture-local state share a test executable.
//! Process-wide measurements, such as the directory I/O budget, stay isolated.

mod cancelled_turns;
mod cost_aggregation;
mod openai_stream_terminals;
mod outbound;
mod priced_exits;
mod read_file_persistence;
mod session_accounts;
