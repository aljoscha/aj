use std::pin::pin;

use anthropic_sdk::messages::{ContentBlock, Message, MessageParam, Messages};
use futures::{Stream, StreamExt};
use nu_ansi_term::Color::{Blue, Red, Yellow};
use serde_json::Value;
use tracing_subscriber::EnvFilter;

/// A harness that's setting up our logging and environment variables and calls
/// into our "real" `run()`.
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    let result = run().await;

    if let Err(err) = result {
        println!("{}: {err}", Red.paint("Error:"));
    }
}

async fn run() -> Result<(), anyhow::Error> {
    let mut agent = Agent::new(StdinUserMessage);

    agent.run().await?;

    Ok(())
}

/// Trait for getting the next message from the user, for passing to the model.
trait GetUserMessage {
    fn get_user_message(&self) -> Option<String>;
}

/// A [GetUserMessage] that reads user messages from stdin.
struct StdinUserMessage;

impl GetUserMessage for StdinUserMessage {
    fn get_user_message(&self) -> Option<String> {
        use std::io::{self, Write};

        io::stdout().flush().unwrap();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) => None, // EOF (ctrl-d)
            Ok(_) => Some(input.trim().to_string()),
            Err(_) => None, // Error (ctrl-c or other)
        }
    }
}

struct Agent<U: GetUserMessage> {
    client: anthropic_sdk::client::Client,
    get_user_message: U,
}

impl<U: GetUserMessage> Agent<U> {
    fn new(get_user_message: U) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let client = anthropic_sdk::client::Client::new(api_key.clone());

        Self {
            client,
            get_user_message,
        }
    }

    async fn run(&mut self) -> Result<(), anyhow::Error> {
        let mut conversation: Vec<MessageParam> = Vec::new();

        println!("Chat with AJ (use 'ctrl-c' to quit)");

        loop {
            print!("{}: ", Blue.paint("You"));
            let user_input = self.get_user_message.get_user_message();
            let user_input = if let Some(user_input) = user_input {
                user_input
            } else {
                break;
            };

            let user_message =
                MessageParam::new_user_message(ContentBlock::new_text_block(user_input));
            conversation.push(user_message);

            let response = self.run_inference_streaming(conversation.clone()).await?;

            print!("{}: ", Yellow.paint("Claude"));
            let mut response = pin!(response);
            while let Some(response) = response.next().await {
                // let response = response:
                println!("{:?}", response,);
            }
            println!();

            let response = self.run_inference(conversation.clone()).await?;

            for content in response.content.iter() {
                match content {
                    ContentBlock::TextBlock { text } => {
                        println!("{}: {}", Yellow.paint("Claude"), text);
                    }
                    other => {
                        println!("{}: {:?}", Yellow.paint("Claude"), other);
                    }
                }
            }

            conversation.push(response.into_message_param());
        }

        Ok(())
    }

    async fn run_inference(
        &self,
        conversation: Vec<MessageParam>,
    ) -> Result<Message, anyhow::Error> {
        let messages = Messages {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 1024,
            messages: conversation,
            ..Default::default()
        };
        let response = self.client.messages(messages).await?;

        Ok(response)
    }

    async fn run_inference_streaming(
        &self,
        conversation: Vec<MessageParam>,
    ) -> Result<impl Stream<Item = StreamingEvent>, anyhow::Error> {
        let messages = Messages {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 1024,
            messages: conversation,
            ..Default::default()
        };
        let response = self.client.messages_stream(messages).await?;

        Ok(response)
    }
}
