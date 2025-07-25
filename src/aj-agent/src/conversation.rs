use aj_conf::{Config, ConfigError};
use aj_ui::UserOutput;
use anthropic_sdk::messages::{ContentBlockParam, MessageParam, Role};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
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

/// An entry in the conversation that can represent different types of
/// interaction data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConversationEntry {
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
        self.entries
            .push(ConversationEntry::Message(ConversationMessage {
                role,
                content,
            }));
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
        self.entries
            .push(ConversationEntry::UserOutput(user_output));
    }

    /// Convert the conversation to a `Vec<MessageParam>` for API calls. This
    /// extracts only the Message entries and converts them to the format
    /// expected by the API
    pub fn to_message_params(&self) -> Vec<MessageParam> {
        self.entries
            .iter()
            .filter_map(|entry| match entry {
                ConversationEntry::Message(msg) => Some(MessageParam {
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
            .filter(|entry| matches!(entry, ConversationEntry::Message(_)))
            .count()
    }

    /// Get the last message in the conversation, if any
    pub fn last_message(&self) -> Option<&ConversationMessage> {
        self.entries.iter().rev().find_map(|entry| match entry {
            ConversationEntry::Message(msg) => Some(msg),
            _ => None,
        })
    }

    /// Get the last user message in the conversation, if any
    pub fn last_user_message(&self) -> Option<&ConversationMessage> {
        self.entries.iter().rev().find_map(|entry| match entry {
            ConversationEntry::Message(msg) => match msg.role {
                Role::User => Some(msg),
                _ => None,
            },
            _ => None,
        })
    }

    /// Get the last assistant message in the conversation, if any
    pub fn last_assistant_message(&self) -> Option<&ConversationMessage> {
        self.entries.iter().rev().find_map(|entry| match entry {
            ConversationEntry::Message(msg) => match msg.role {
                Role::Assistant => Some(msg),
                _ => None,
            },
            _ => None,
        })
    }

    /// Save the conversation to a JSONL file in the threads directory
    pub fn save_to_file(&self) -> Result<PathBuf, ConversationError> {
        let threads_dir = Config::get_threads_dir_path()?;
        let file_path = threads_dir.join(format!("{}.jsonl", self.conversation_id));
        
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file_path)?;

        // Write each entry as a separate JSON line
        for entry in &self.entries {
            let json_line = serde_json::to_string(entry)?;
            writeln!(file, "{json_line}")?;
        }

        Ok(file_path)
    }

    /// Restore a conversation from a JSONL file
    pub fn restore_from_file(conversation_id: &str) -> Result<Self, ConversationError> {
        let threads_dir = Config::get_threads_dir_path()?;
        let file_path = threads_dir.join(format!("{conversation_id}.jsonl"));
        
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

        Ok(Self {
            conversation_id: conversation_id.to_string(),
            entries,
        })
    }

    /// Get the file path where this conversation would be saved
    pub fn get_file_path(&self) -> Result<PathBuf, ConversationError> {
        let threads_dir = Config::get_threads_dir_path()?;
        Ok(threads_dir.join(format!("{}.jsonl", self.conversation_id)))
    }
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new()
    }
}
