use aj_ui::AjUi;
use console::{Color, style};
use similar::{ChangeTag, TextDiff};
use termimad::{Alignment, MadSkin};

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

        print!("{}: ", style("you").bold().fg(Color::Blue));

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

    fn agent_text_start(&self, _text: &str) {
        print!("{}:\n", style("aj").bold().fg(Color::Yellow));
    }

    fn agent_text_update(&self, _diff: &str) {
        // print!("{}", diff);
    }

    fn agent_text_stop(&self, text: &str) {
        render_markdown(text);
        println!();
    }

    fn agent_thinking_start(&self, thinking: &str) {
        print!(
            "{}: {}",
            style("aj is thinking").bold().fg(DARK_GRAY),
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
        println!("{}({})", style(tool_name).bold().fg(Color::Green), input);

        println!("{}\n", result);
    }

    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str) {
        println!("{}({})", style(tool_name).bold().fg(Color::Green), input);

        let diff = TextDiff::from_lines(before, after);
        const CONTEXT_LINES: usize = 3;

        // Collect all changes with line numbers
        let changes: Vec<_> = diff.iter_all_changes().enumerate().collect();

        // Find ranges of changes with context
        let mut ranges = Vec::new();
        let mut current_start = None;

        for (i, change) in &changes {
            match change.tag() {
                ChangeTag::Delete | ChangeTag::Insert => {
                    if current_start.is_none() {
                        current_start = Some(i.saturating_sub(CONTEXT_LINES));
                    }
                }
                ChangeTag::Equal => {
                    if let Some(start) = current_start {
                        ranges.push((start, (i + CONTEXT_LINES).min(changes.len())));
                        current_start = None;
                    }
                }
            }
        }

        // Handle case where diff ends with changes
        if let Some(start) = current_start {
            ranges.push((start, changes.len()));
        }

        // Merge overlapping ranges
        ranges.sort_by_key(|(start, _)| *start);
        let mut merged_ranges = Vec::new();
        for (start, end) in ranges {
            if let Some((_, last_end)) = merged_ranges.last_mut() {
                if start <= *last_end {
                    *last_end = (*last_end).max(end);
                    continue;
                }
            }
            merged_ranges.push((start, end));
        }

        let mut old_line_num = 1;
        let mut new_line_num = 1;
        let mut is_first_range = true;

        for (range_start, range_end) in &merged_ranges {
            // Show separator if not the first range
            if !is_first_range && *range_start > 0 {
                println!("{}", style("...").dim());
            }
            is_first_range = false;

            // Calculate line numbers at range start
            for (i, _) in changes.iter().take(*range_start) {
                let change = &changes[*i].1;
                match change.tag() {
                    ChangeTag::Delete => old_line_num += 1,
                    ChangeTag::Insert => new_line_num += 1,
                    ChangeTag::Equal => {
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            }

            // Display changes in this range
            for (_, change) in changes.iter().take(*range_end).skip(*range_start) {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };

                let line_num_str = match change.tag() {
                    ChangeTag::Delete => format!("{:4}     ", old_line_num),
                    ChangeTag::Insert => format!("     {:4}", new_line_num),
                    ChangeTag::Equal => format!("{:4}:{:4}", old_line_num, new_line_num),
                };

                let styled_line = match change.tag() {
                    ChangeTag::Delete => style(format!(
                        "{} {} {}",
                        line_num_str,
                        sign,
                        change.value().trim_end()
                    ))
                    .bg(Color::Red)
                    .on_bright()
                    .black(),
                    ChangeTag::Insert => style(format!(
                        "{} {} {}",
                        line_num_str,
                        sign,
                        change.value().trim_end()
                    ))
                    .bg(Color::Green)
                    .on_bright()
                    .black(),
                    ChangeTag::Equal => style(format!(
                        "{} {} {}",
                        line_num_str,
                        sign,
                        change.value().trim_end()
                    ))
                    .dim(),
                };

                println!("{}", styled_line);

                // Update line numbers
                match change.tag() {
                    ChangeTag::Delete => old_line_num += 1,
                    ChangeTag::Insert => new_line_num += 1,
                    ChangeTag::Equal => {
                        old_line_num += 1;
                        new_line_num += 1;
                    }
                }
            }
        }
        println!();
    }

    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str) {
        println!("{}({})", style(tool_name).bold().fg(Color::Green), input);
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

fn render_markdown(text: &str) {
    let mut skin = MadSkin::default_light();
    skin.headers[0].align = Alignment::Left;
    skin.headers[0].add_attr(termimad::crossterm::style::Attribute::Underlined);
    skin.headers[0].add_attr(termimad::crossterm::style::Attribute::Bold);

    skin.headers[1].add_attr(termimad::crossterm::style::Attribute::NoUnderline);
    skin.headers[1].add_attr(termimad::crossterm::style::Attribute::Bold);

    skin.headers[2].add_attr(termimad::crossterm::style::Attribute::NoUnderline);
    skin.headers[2].add_attr(termimad::crossterm::style::Attribute::Bold);

    skin.inline_code
        .set_fg(termimad::crossterm::style::Color::Blue);
    skin.inline_code
        .set_bg(termimad::crossterm::style::Color::White);

    skin.print_text(text);
}
