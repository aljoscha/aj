use aj_conf::ConfigError;
use aj_ui::UserOutput;
use anthropic_sdk::messages::{ContentBlockParam, MessageParam, Role};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConversationError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Config error: {0}")]
    Config(#[from] ConfigError),
}

/// A conversation represents the full interaction history between the user and
/// agent, including messages, tool outputs, and UI display content for
/// potential replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    conversation_id: String,
    entries: Vec<ConversationEntry>,
}

/// An entry in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub entry: ConversationEntryKind,
}

/// The different types of conversation entries
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEntryKind {
    /// A message exchanged between user and assistant (maps to MessageParam)
    Message(ConversationMessage),
    /// Information that is displayed to the user.
    UserOutput(UserOutput),
}

/// Internal representation of a conversation message that maps to MessageParam
// NOTE: Once we want to support different model APIs/providers, we'll have to
// make this independent of the Anthropic SDK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: Role,
    pub content: Vec<ContentBlockParam>,
}

impl Conversation {
    /// Create a new empty conversation
    pub fn new() -> Self {
        let now: DateTime<Utc> = Utc::now();
        let conversation_id = now.format("%Y-%m-%d-%H-%M-%S").to_string();

        Self {
            conversation_id,
            entries: Vec::new(),
        }
    }

    /// Add a message to the conversation
    pub fn add_message(&mut self, role: Role, content: Vec<ContentBlockParam>) {
        self.entries.push(ConversationEntry {
            timestamp: Some(Utc::now()),
            entry: ConversationEntryKind::Message(ConversationMessage { role, content }),
        });
    }

    /// Add a user message to the conversation
    pub fn add_user_message(&mut self, content: Vec<ContentBlockParam>) {
        self.add_message(Role::User, content);
    }

    /// Add an assistant message to the conversation
    pub fn add_assistant_message(&mut self, content: Vec<ContentBlockParam>) {
        self.add_message(Role::Assistant, content);
    }

    /// Add user output to the conversation
    pub fn add_user_output(&mut self, user_output: UserOutput) {
        self.entries.push(ConversationEntry {
            timestamp: Some(Utc::now()),
            entry: ConversationEntryKind::UserOutput(user_output),
        });
    }

    /// Convert the conversation to a `Vec<MessageParam>` for API calls. This
    /// extracts only the Message entries and converts them to the format
    /// expected by the API
    pub fn to_message_params(&self) -> Vec<MessageParam> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.entry {
                ConversationEntryKind::Message(msg) => Some(MessageParam {
                    role: msg.role.clone(),
                    content: msg.content.clone(),
                }),
                // Future entries like ToolOutput and UiDisplay are not included in API calls
                _ => None,
            })
            .collect()
    }

    /// Get the conversation ID
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// Get all entries in the conversation
    pub fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    /// Get mutable access to all entries (for advanced use cases)
    pub fn entries_mut(&mut self) -> &mut Vec<ConversationEntry> {
        &mut self.entries
    }

    /// Get the number of total entries in the conversation
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the conversation is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the number of message entries only (excluding tool output and UI
    /// display)
    pub fn message_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.entry, ConversationEntryKind::Message(_)))
            .count()
    }

    /// Get the last message in the conversation, if any
    pub fn last_message(&self) -> Option<&ConversationMessage> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message(msg) => Some(msg),
                _ => None,
            })
    }

    /// Get the last user message in the conversation, if any
    pub fn last_user_message(&self) -> Option<&ConversationMessage> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message(msg) => match msg.role {
                    Role::User => Some(msg),
                    _ => None,
                },
                _ => None,
            })
    }

    /// Get the last assistant message in the conversation, if any
    pub fn last_assistant_message(&self) -> Option<&ConversationMessage> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message(msg) => match msg.role {
                    Role::Assistant => Some(msg),
                    _ => None,
                },
                _ => None,
            })
    }
}

/// Handles persistence operations for conversations, including loading,
/// listing, and managing conversation threads in a specified directory.
#[derive(Clone)]
pub struct ConversationPersistence {
    threads_dir: PathBuf,
}

impl ConversationPersistence {
    /// Create a new [ConversationPersistence] instance with the given threads
    /// directory.
    pub fn new(threads_dir: PathBuf) -> Self {
        Self { threads_dir }
    }

    /// Get metadata about all conversation threads, sorted by creation time
    /// (latest first)
    pub fn list_threads(&self) -> Result<Vec<ThreadMetadata>, ConversationError> {
        if !self.threads_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.threads_dir)?;
        let mut thread_files = Vec::new();

        // Collect all .jsonl files
        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                    thread_files.push(file_stem.to_string());
                }
            }
        }

        // Sort by filename (which corresponds to creation time), latest first
        thread_files.sort_by(|a, b| b.cmp(a));

        let mut threads = Vec::new();

        for thread_id in thread_files {
            let path = self.threads_dir.join(format!("{thread_id}.jsonl"));

            // Get file metadata for additional info
            let metadata = fs::metadata(&path)?;
            let modified = metadata.modified()?;
            let modified_str = DateTime::<Utc>::from(modified)
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string();

            // Use file size as proxy for conversation length
            let file_size = metadata.len();
            let size_display = if file_size < 1024 {
                format!("{file_size}B")
            } else if file_size < 1024 * 1024 {
                format!("{}KB", file_size / 1024)
            } else {
                format!("{}MB", file_size / (1024 * 1024))
            };

            threads.push(ThreadMetadata {
                thread_id,
                modified: modified_str,
                size_display,
            });
        }

        Ok(threads)
    }

    /// Load a conversation from the threads directory
    pub fn load_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Conversation, ConversationError> {
        let file_path = self.threads_dir.join(format!("{conversation_id}.jsonl"));

        let file = File::open(file_path)?;
        let reader = BufReader::new(file);

        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if !line.trim().is_empty() {
                let entry: ConversationEntry = serde_json::from_str(&line)?;
                entries.push(entry);
            }
        }

        Ok(Conversation {
            conversation_id: conversation_id.to_string(),
            entries,
        })
    }

    /// Get the latest conversation thread ID, if any exist
    pub fn get_latest_thread_id(&self) -> Result<Option<String>, ConversationError> {
        let threads = self.list_threads()?;
        Ok(threads.first().map(|t| t.thread_id.clone()))
    }

    /// Save a conversation to the threads directory
    pub fn save_conversation(
        &self,
        conversation: &Conversation,
    ) -> Result<PathBuf, ConversationError> {
        // Ensure the threads directory exists
        if !self.threads_dir.exists() {
            fs::create_dir_all(&self.threads_dir)?;
        }

        let file_path = self
            .threads_dir
            .join(format!("{}.jsonl", conversation.conversation_id()));

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file_path)?;

        // Write each entry as a separate JSON line
        for entry in conversation.entries() {
            let json_line = serde_json::to_string(entry)?;
            writeln!(file, "{json_line}")?;
        }

        Ok(file_path)
    }
}

/// Metadata about a conversation thread
#[derive(Debug, Clone)]
pub struct ThreadMetadata {
    pub thread_id: String,
    pub modified: String,
    pub size_display: String,
}
