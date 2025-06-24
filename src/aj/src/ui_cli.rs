use aj_ui::AjUi;
use console::{Color, style};
use similar::{ChangeTag, TextDiff};

/// Cli-based implementation of [AjUi].
pub struct AjCli;

impl Default for AjCli {
    fn default() -> Self {
        Self::new()
    }
}

impl AjCli {
    pub fn new() -> Self {
        Self
    }
}

const DARK_GRAY: Color = Color::Color256(239);
const LIGHT_GRAY: Color = Color::Color256(248);

impl AjUi for AjCli {
    fn display_notice(&self, notice: &str) {
        println!("{}\n", notice);
    }

    fn display_error(&self, error: &str) {
        println!("{}: {}\n", style("Error:").bold().fg(Color::Red), error);
    }

    fn get_user_input(&self) -> Option<String> {
        use std::io::{self, Write};

        print!("{}: ", style("you").fg(Color::Blue));

        io::stdout().flush().unwrap();

        let mut input = String::new();
        let input = match io::stdin().read_line(&mut input) {
            Ok(0) => None, // EOF (ctrl-d)
            Ok(_) => Some(input.trim().to_string()),
            Err(_) => None, // Error (ctrl-c or other)
        };

        println!();

        input
    }

    fn agent_text_start(&self, text: &str) {
        print!("{}: {}", style("aj").fg(Color::Yellow), text);
    }

    fn agent_text_update(&self, diff: &str) {
        print!("{}", diff);
    }

    fn agent_text_stop(&self) {
        println!("\n");
    }

    fn agent_thinking_start(&self, thinking: &str) {
        print!(
            "{}: {}",
            style("aj is thinking").fg(DARK_GRAY),
            style(thinking).fg(LIGHT_GRAY).on_bright()
        );
    }

    fn agent_thinking_update(&self, diff: &str) {
        print!("{}", diff);
    }

    fn agent_thinking_stop(&self) {
        println!("\n");
    }

    fn display_tool_result(&self, tool_name: &str, input: &str, result: &str) {
        println!(
            "{}: {}({})",
            style("tool").fg(Color::Green),
            tool_name,
            input
        );

        println!("{}\n", result);
    }

    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str) {
        println!(
            "{}: {}({})",
            style("tool").fg(Color::Green),
            tool_name,
            input
        );

        let diff = TextDiff::from_lines(before, after);

        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };

            let styled_line = match change.tag() {
                ChangeTag::Delete => style(format!("{} {}", sign, change.value().trim_end()))
                    .bg(Color::Red)
                    .on_bright()
                    .black(),
                ChangeTag::Insert => style(format!("{} {}", sign, change.value().trim_end()))
                    .bg(Color::Green)
                    .on_bright()
                    .black(),
                ChangeTag::Equal => style(format!("{} {}", sign, change.value().trim_end())).dim(),
            };

            println!("{}", styled_line);
        }
        println!();
    }

    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str) {
        println!(
            "{}: {}({})",
            style("tool").fg(Color::Green),
            tool_name,
            input
        );
        println!("{}: {}", style("tool_error").bold().fg(Color::Red), error);
    }

    fn ask_permission(&self, message: &str) -> bool {
        use std::io::{self, Write};

        println!("\n{}", message);
        print!("Allow this command? (y/n): ");
        io::stdout().flush().unwrap();

        let mut user_input = String::new();
        io::stdin().read_line(&mut user_input).unwrap();
        let user_input = user_input.trim().to_lowercase();

        println!();

        user_input == "y" || user_input == "yes"
    }
}
