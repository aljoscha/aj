//! Types for a higher-level streaming API built on the low-lever server-sent
//! events defined in the messages API.

use async_stream::stream;
use futures::Stream;

use crate::messages::{
    ApiError, Citation, ContentBlock, ContentBlockDelta, Message, ServerSentEvent, UsageDelta,
};

// High-level streaming events with both diff updates and a running snapshot of
// the accumulated message.
#[derive(Clone, Debug)]
pub enum StreamingEvent {
    MessageStart {
        message: Message,
    },
    UsageUpdate {
        usage: UsageDelta,
    },
    FinalizedMessage {
        message: Message,
    },
    Error {
        error: ApiError,
    },
    TextStart {
        text: String,
        citations: Vec<Citation>,
    },
    TextUpdate {
        diff: String,
        snapshot: String,
    },
    TextStop {
        /// The complete text block.
        text: String,
    },
    ThinkingStart {
        thinking: String,
    },
    ThinkingUpdate {
        diff: String,
        snapshot: String,
    },
    ThinkingStop,
    ParseError {
        error: String,
        raw_data: String,
    },
    ProtocolError {
        error: String,
    },
}

#[derive(Debug)]
pub struct StreamProcessor {
    current_message: Option<Message>,
    current_content_block: Option<ContentBlock>,
    current_json: Option<String>,
}

impl StreamProcessor {
    pub fn new() -> Self {
        Self {
            current_message: None,
            current_content_block: None,
            current_json: None,
        }
    }

    pub fn process_stream<S>(mut self, stream: S) -> impl Stream<Item = StreamingEvent>
    where
        S: Stream<Item = ServerSentEvent>,
    {
        stream! {
            let mut stream = std::pin::pin!(stream);

            while let Some(event) = futures::StreamExt::next(&mut stream).await {
                match self.process_event(event) {
                    Some(high_level_event) => yield high_level_event,
                    None => continue,
                }
            }
        }
    }

    pub fn process_event(&mut self, event: ServerSentEvent) -> Option<StreamingEvent> {
        match event {
            ServerSentEvent::MessageStart { message } => {
                let prev = self.current_message.replace(message.clone());
                if prev.is_some() {
                    return Some(StreamingEvent::ProtocolError {
                        error: "got new message start while we're already assembling a message"
                            .to_string(),
                    });
                }
                Some(StreamingEvent::MessageStart { message })
            }
            ServerSentEvent::MessageDelta { delta, usage } => {
                let message = match self.current_message.as_mut() {
                    Some(msg) => msg,
                    None => {
                        return Some(StreamingEvent::ProtocolError {
                            error: "got message delta but don't have a current message".to_string(),
                        });
                    }
                };

                message.stop_reason = delta.stop_reason;
                message.stop_sequence = delta.stop_sequence;

                message.usage.update(&usage);

                Some(StreamingEvent::UsageUpdate { usage })
            }
            ServerSentEvent::MessageStop => {
                let message = match self.current_message.take() {
                    Some(msg) => msg,
                    None => {
                        return Some(StreamingEvent::ProtocolError {
                            error: "got message stop but don't have a current message".to_string(),
                        });
                    }
                };
                Some(StreamingEvent::FinalizedMessage { message })
            }
            ServerSentEvent::Ping => {
                // ignoring these
                None
            }
            ServerSentEvent::Error { error } => Some(StreamingEvent::Error { error }),

            ServerSentEvent::ContentBlockStart {
                index: _,
                content_block,
            } => {
                let prev = self.current_content_block.replace(content_block.clone());
                if prev.is_some() {
                    return Some(StreamingEvent::ProtocolError {
                        error: "got new content block start while we're already assembling a content block".to_string(),
                    });
                }
                match content_block {
                    ContentBlock::TextBlock { text, citations } => {
                        Some(StreamingEvent::TextStart { text, citations })
                    }
                    ContentBlock::ThinkingBlock { thinking, .. } => {
                        Some(StreamingEvent::ThinkingStart { thinking })
                    }
                    _ => {
                        self.current_json = Some(String::new());
                        None
                    }
                }
            }

            ServerSentEvent::ContentBlockDelta { index, delta } => {
                let content_block = match self.current_content_block.as_mut() {
                    Some(block) => block,
                    None => {
                        return Some(StreamingEvent::ProtocolError {
                            error: "got content block delta but don't have a current content block"
                                .to_string(),
                        });
                    }
                };

                let current_message = match self.current_message.as_ref() {
                    Some(msg) => msg,
                    None => {
                        return Some(StreamingEvent::ProtocolError {
                            error: "got content block delta but don't have a current message"
                                .to_string(),
                        });
                    }
                };

                if current_message.content.len() != index as usize {
                    return Some(StreamingEvent::ProtocolError {
                        error: format!(
                            "got content block delta for index {} but current message has {} content blocks",
                            index,
                            current_message.content.len()
                        ),
                    });
                }

                match delta {
                    ContentBlockDelta::TextDelta { text } => {
                        let current_text = match content_block {
                            ContentBlock::TextBlock { text, .. } => text,
                            _ => {
                                return Some(StreamingEvent::ProtocolError {
                                    error: "got text delta for non-text content block".to_string(),
                                });
                            }
                        };
                        current_text.push_str(&text);
                        Some(StreamingEvent::TextUpdate {
                            diff: text,
                            snapshot: current_text.clone(),
                        })
                    }
                    ContentBlockDelta::ThinkingDelta { thinking } => {
                        let current_thinking = match content_block {
                            ContentBlock::ThinkingBlock { thinking, .. } => thinking,
                            _ => {
                                return Some(StreamingEvent::ProtocolError {
                                    error: "got thinking delta for non-thinking content block"
                                        .to_string(),
                                });
                            }
                        };
                        current_thinking.push_str(&thinking);
                        Some(StreamingEvent::ThinkingUpdate {
                            diff: thinking,
                            snapshot: current_thinking.clone(),
                        })
                    }
                    ContentBlockDelta::SignatureDelta { signature } => {
                        let current_signature = match content_block {
                            ContentBlock::ThinkingBlock { signature, .. } => signature,
                            _ => {
                                return Some(StreamingEvent::ProtocolError {
                                    error: "got signature delta for non-thinking content block"
                                        .to_string(),
                                });
                            }
                        };
                        current_signature.push_str(&signature);
                        None
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        let current_json = match self.current_json.as_mut() {
                            Some(json) => json,
                            None => {
                                return Some(StreamingEvent::ProtocolError {
                                    error: "got input json delta but don't have a current json"
                                        .to_string(),
                                });
                            }
                        };
                        current_json.push_str(&partial_json);
                        None
                    }
                }
            }

            ServerSentEvent::ContentBlockStop { index: _ } => {
                let content_block = match self.current_content_block.take() {
                    Some(block) => block,
                    None => {
                        return Some(StreamingEvent::ProtocolError {
                            error: "got content block stop but don't have a current content block"
                                .to_string(),
                        });
                    }
                };

                let current_message = match self.current_message.as_mut() {
                    Some(msg) => msg,
                    None => {
                        return Some(StreamingEvent::ProtocolError {
                            error: "got content block stop but don't have a current message"
                                .to_string(),
                        });
                    }
                };

                match content_block {
                    ContentBlock::TextBlock { text, citations } => {
                        current_message.content.push(ContentBlock::TextBlock {
                            text: text.clone(),
                            citations,
                        });
                        Some(StreamingEvent::TextStop { text })
                    }
                    thinking_block @ ContentBlock::ThinkingBlock { .. } => {
                        current_message.content.push(thinking_block);
                        Some(StreamingEvent::ThinkingStop)
                    }
                    ContentBlock::ToolUseBlock { id, name, input: _ } => {
                        let input_json = match self.current_json.take() {
                            Some(json) => json,
                            None => {
                                return Some(StreamingEvent::ProtocolError {
                                    error: "got tool use but don't have a current json".to_string(),
                                });
                            }
                        };
                        let input = match serde_json::from_str(&input_json) {
                            Ok(input) => input,
                            Err(e) => {
                                return Some(StreamingEvent::ParseError {
                                    error: format!("failed to parse tool use input json: {}", e),
                                    raw_data: input_json,
                                });
                            }
                        };
                        current_message.content.push(ContentBlock::ToolUseBlock {
                            id,
                            name,
                            input,
                        });
                        None
                    }
                    ContentBlock::ServerToolUseBlock { id, name, input: _ } => {
                        let input_json = match self.current_json.take() {
                            Some(json) => json,
                            None => {
                                return Some(StreamingEvent::ProtocolError {
                                    error: "got server tool use but don't have a current json"
                                        .to_string(),
                                });
                            }
                        };
                        let input = match serde_json::from_str(&input_json) {
                            Ok(input) => input,
                            Err(e) => {
                                return Some(StreamingEvent::ParseError {
                                    error: format!(
                                        "failed to parse server tool use input json: {}",
                                        e
                                    ),
                                    raw_data: input_json,
                                });
                            }
                        };
                        current_message
                            .content
                            .push(ContentBlock::ServerToolUseBlock { id, name, input });
                        None
                    }
                    ContentBlock::MCPToolUseBlock {
                        id,
                        name,
                        server_name,
                        input: _,
                    } => {
                        let input_json = match self.current_json.take() {
                            Some(json) => json,
                            None => {
                                return Some(StreamingEvent::ProtocolError {
                                    error: "got mcp tool use but don't have a current json"
                                        .to_string(),
                                });
                            }
                        };
                        let input = match serde_json::from_str(&input_json) {
                            Ok(input) => input,
                            Err(e) => {
                                return Some(StreamingEvent::ParseError {
                                    error: format!(
                                        "failed to parse mcp tool use input json: {}",
                                        e
                                    ),
                                    raw_data: input_json,
                                });
                            }
                        };
                        current_message.content.push(ContentBlock::MCPToolUseBlock {
                            id,
                            name,
                            server_name,
                            input,
                        });
                        None
                    }
                    other => {
                        current_message.content.push(other);
                        None
                    }
                }
            }
        }
    }
}

impl Default for StreamProcessor {
    fn default() -> Self {
        Self::new()
    }
}
