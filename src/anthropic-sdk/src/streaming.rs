//! Types for a higher-level streaming API built on the low-lever server-sent
//! events defined in the messages API.

use async_stream::stream;
use futures::Stream;

use crate::messages::{
    ApiError, ContentBlock, ContentBlockDelta, Message, ServerSentEvent, UsageDelta,
};

// High-level streaming events with both diff updates and a running snapshot of
// the accumulated message.
#[derive(Clone, Debug)]
pub enum StreamingEvent {
    MessageStart { message: Message },
    UsageUpdate { usage: UsageDelta },
    FinalizedMessage { message: Message },
    Error { error: ApiError },
    TextStart { text: String },
    TextUpdate { diff: String, snapshot: String },
    TextStop,
    ThinkingStart { thinking: String },
    ThinkingUpdate { diff: String, snapshot: String },
    ThinkingStop,
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

    pub fn process_stream<S>(
        &mut self,
        stream: S,
    ) -> impl Stream<Item = StreamingEvent> + use<'_, S>
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
                assert!(
                    prev.is_none(),
                    "got new message start while we're already assembling a message"
                );
                Some(StreamingEvent::MessageStart { message })
            }
            ServerSentEvent::MessageDelta { delta, usage } => {
                let message = self
                    .current_message
                    .as_mut()
                    .expect("got message delta but don't have a current message");

                message.stop_reason = delta.stop_reason;
                message.stop_sequence = delta.stop_sequence;

                message.usage.update(&usage);

                Some(StreamingEvent::UsageUpdate { usage })
            }
            ServerSentEvent::MessageStop => {
                let message = self
                    .current_message
                    .take()
                    .expect("got message stop but don't have a current message");
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
                assert!(
                    prev.is_none(),
                    "got new content block start while we're already assembling a content block"
                );
                match content_block {
                    ContentBlock::TextBlock { text } => Some(StreamingEvent::TextStart { text }),
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
                let content_block = self
                    .current_content_block
                    .as_mut()
                    .expect("got content block delta but don't have a current content block");

                let current_message = self
                    .current_message
                    .as_ref()
                    .expect("got content block delta but don't have a current message");

                assert!(
                    current_message.content.len() == index as usize,
                    "got
                content block delta for index {} but current message has {}
                content blocks",
                    index,
                    current_message.content.len()
                );

                match delta {
                    ContentBlockDelta::TextDelta { text } => {
                        let current_text = match content_block {
                            ContentBlock::TextBlock { text } => text,
                            _ => panic!("got text delta for non-text content block"),
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
                            _ => panic!("got thinking delta for non-thinking content block"),
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
                            _ => panic!("got signature delta for non-thinking content block"),
                        };
                        current_signature.push_str(&signature);
                        None
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        let current_json = self
                            .current_json
                            .as_mut()
                            .expect("got input json delta but don't have a current json");
                        current_json.push_str(&partial_json);
                        None
                    }
                }
            }

            ServerSentEvent::ContentBlockStop { index: _ } => {
                let content_block = self
                    .current_content_block
                    .take()
                    .expect("got content block stop but don't have a current content block");

                let current_message = self
                    .current_message
                    .as_mut()
                    .expect("got content block stop but don't have a current message");

                match content_block {
                    text_block @ ContentBlock::TextBlock { .. } => {
                        current_message.content.push(text_block);
                        Some(StreamingEvent::TextStop)
                    }
                    thinking_block @ ContentBlock::ThinkingBlock { .. } => {
                        current_message.content.push(thinking_block);
                        Some(StreamingEvent::ThinkingStop)
                    }
                    ContentBlock::ToolUseBlock { id, name, input: _ } => {
                        let input_json = self
                            .current_json
                            .take()
                            .expect("got tool use but don't have a current json");
                        let input = serde_json::from_str(&input_json)
                            .expect("failed to parse tool use input json");
                        current_message.content.push(ContentBlock::ToolUseBlock {
                            id,
                            name,
                            input,
                        });
                        None
                    }
                    ContentBlock::ServerToolUseBlock { id, name, input: _ } => {
                        let input_json = self
                            .current_json
                            .take()
                            .expect("got server tool use but don't have a current json");
                        let input = serde_json::from_str(&input_json)
                            .expect("failed to parse server tool use input json");
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
                        let input_json = self
                            .current_json
                            .take()
                            .expect("got mcp tool use but don't have a current json");
                        let input = serde_json::from_str(&input_json)
                            .expect("failed to parse mcp tool use input json");
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
