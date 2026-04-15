use aj_ui::{SubAgentUsage, TokenUsage, UsageSummary};
use console::{Color, style};
use similar::{ChangeTag, TextDiff};
use termimad::{Alignment, MadSkin};

use crate::cli::{DARK_GRAY, LIGHT_GRAY};

/// Common CLI functionality shared between main CLI and sub-agent CLI.
#[derive(Clone)]
pub struct AjCliCommon {
    /// Optional prefix to add to output (e.g., "(sub agent 1)").
    prefix: Option<String>,
    /// Whether to show full tool output or just headers.
    show_full_tool_output: bool,
    /// Whether to enable streaming updates.
    enable_streaming: bool,
}

impl AjCliCommon {
    pub fn new(
        prefix: Option<String>,
        show_full_tool_output: bool,
        enable_streaming: bool,
    ) -> Self {
        Self {
            prefix,
            show_full_tool_output,
            enable_streaming,
        }
    }

    fn format_with_prefix(&self, message: &str) -> String {
        if let Some(prefix) = &self.prefix {
            format!("{prefix} {message}")
        } else {
            message.to_string()
        }
    }

    pub fn display_notice(&self, notice: &str) {
        let formatted = self.format_with_prefix(notice);
        println!("{formatted}\n");
    }

    pub fn display_warning(&self, warning: &str) {
        let formatted = self.format_with_prefix(warning);
        println!("{}\n", style(formatted).bold().fg(Color::Yellow));
    }

    pub fn display_error(&self, error: &str) {
        if let Some(prefix) = &self.prefix {
            println!("{prefix} Error: {error}");
        } else {
            println!("{}: {}\n", style("error").bold().fg(Color::Red), error);
        }
    }

    pub fn agent_text_start(&self, _text: &str) {
        if let Some(prefix) = &self.prefix {
            println!("{}:", style(prefix).bold().fg(Color::Yellow));
        } else {
            println!("{}:", style("aj").bold().fg(Color::Yellow));
        }
    }

    pub fn agent_text_update(&self, _diff: &str) {
        if self.enable_streaming {
            // print!("{}", diff);
        }
    }

    pub fn agent_text_stop(&self, text: &str) {
        render_markdown(text);
        println!();
    }

    pub fn user_text_start(&self, _text: &str) {
        if let Some(prefix) = &self.prefix {
            println!("{prefix} {}:", style("you").bold().fg(Color::Blue));
        } else {
            println!("{}:", style("you").bold().fg(Color::Blue));
        }
    }

    pub fn user_text_update(&self, _diff: &str) {
        // No-op for user text updates
    }

    pub fn user_text_stop(&self, text: &str) {
        if let Some(prefix) = &self.prefix {
            println!("{prefix} {text}\n");
        } else {
            println!("{text}\n");
        }
    }

    pub fn agent_thinking_start(&self, thinking: &str) {
        if let Some(prefix) = &self.prefix {
            print!(
                "{prefix} {}: {}",
                style("aj is thinking").bold().fg(DARK_GRAY),
                style(thinking).fg(LIGHT_GRAY).on_bright()
            );
        } else {
            print!(
                "{}: {}",
                style("aj is thinking").bold().fg(DARK_GRAY),
                style(thinking).fg(LIGHT_GRAY).on_bright()
            );
        }
    }

    pub fn agent_thinking_update(&self, diff: &str) {
        print!("{diff}");
    }

    pub fn agent_thinking_stop(&self) {
        println!("\n");
    }

    pub fn display_tool_result(&self, tool_name: &str, input: &str, result: &str) {
        let tool_header = format!("{}({})", style(tool_name).bold().fg(Color::Green), input);

        if let Some(prefix) = &self.prefix {
            println!("{prefix} {tool_header}");
        } else {
            println!("{tool_header}");
        }

        if self.show_full_tool_output {
            let truncated_result = truncate_output(result);
            println!("{truncated_result}\n");
        }
    }

    pub fn display_tool_result_diff(
        &self,
        tool_name: &str,
        input: &str,
        before: &str,
        after: &str,
    ) {
        let tool_header = format!("{}({})", style(tool_name).bold().fg(Color::Green), input);

        if let Some(prefix) = &self.prefix {
            println!("{prefix} {tool_header}");
        } else {
            println!("{tool_header}");
        }

        if self.show_full_tool_output {
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
                        ChangeTag::Delete => format!("{old_line_num:4}     "),
                        ChangeTag::Insert => format!("     {new_line_num:4}"),
                        ChangeTag::Equal => format!("{old_line_num:4}:{new_line_num:4}"),
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

                    println!("{styled_line}");

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
    }

    pub fn display_tool_error(&self, tool_name: &str, input: &str, error: &str) {
        let tool_header = format!("{}({})", style(tool_name).bold().fg(Color::Green), input);

        if let Some(prefix) = &self.prefix {
            println!("{prefix} {tool_header}");
            println!(
                "{prefix} {}: {}\n",
                style("tool_error").bold().fg(Color::Red),
                error
            );
        } else {
            println!("{tool_header}");
            println!("{}: {}\n", style("tool_error").bold().fg(Color::Red), error);
        }
    }

    pub fn ask_permission(&self, message: &str) -> bool {
        ask_user_permission(self.prefix.as_deref(), message)
    }

    pub fn display_token_usage(&self, usage: &TokenUsage) {
        let usage_string = format_token_usage(usage);
        if let Some(prefix) = &self.prefix {
            println!("{} {}\n", style(prefix).dim(), style(usage_string).dim());
        } else {
            println!("{}\n", style(usage_string).dim());
        }
    }

    pub fn display_token_usage_summary(&self, summary: &UsageSummary) {
        let usage_string = format_usage_summary(summary);
        if let Some(prefix) = &self.prefix {
            println!("{} {}\n", style(prefix).dim(), style(usage_string).dim());
        } else {
            println!("{}\n", style(usage_string).dim());
        }
    }
}

pub(crate) fn format_token_usage(usage: &TokenUsage) -> String {
    let format_tokens = |acc: u64, turn: u64| -> String {
        if turn == 0 {
            format!("{acc}")
        } else {
            format!("{acc}+{turn}")
        }
    };

    let input_str = format_tokens(usage.accumulated_input, usage.turn_input);
    let output_str = format_tokens(usage.accumulated_output, usage.turn_output);
    let cache_creation_str =
        format_tokens(usage.accumulated_cache_creation, usage.turn_cache_creation);
    let cache_read_str = format_tokens(usage.accumulated_cache_read, usage.turn_cache_read);

    let usage_string = format!(
        "Token Usage - Input: {input_str} | Output: {output_str} | Cache Creation: {cache_creation_str} | Cache Read: {cache_read_str}",
    );

    usage_string
}

pub(crate) fn format_usage_summary(summary: &UsageSummary) -> String {
    let format_single_usage = |usage: &SubAgentUsage| -> String {
        format!(
            "Input: {} | Output: {} | Cache Creation: {} | Cache Read: {}",
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_tokens,
            usage.cache_read_tokens
        )
    };

    let mut result = String::new();

    // Main agent usage
    result.push_str(&format!(
        "Main Agent - {}\n",
        format_single_usage(&summary.main_agent_usage)
    ));

    // Sub-agent usage
    for sub_usage in &summary.sub_agent_usage {
        if let Some(agent_id) = sub_usage.agent_id {
            result.push_str(&format!(
                "Sub-agent {} - {}\n",
                agent_id,
                format_single_usage(sub_usage)
            ));
        }
    }

    // Total usage
    result.push_str(&format!(
        "TOTAL - {}",
        format_single_usage(&summary.total_usage)
    ));

    result
}

/// Truncates output for display if it's too long, showing first and last
/// portions with an indicator of omitted content in the middle.
pub(crate) fn truncate_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();

    if lines.len() <= 20 {
        // Display all lines
        output.to_string()
    } else {
        // Display first 8 lines
        let mut result = String::new();
        for line in lines.iter().take(8) {
            result.push_str(line);
            result.push('\n');
        }

        // Show omitted indicator with count
        let omitted_lines = lines.len() - 16; // Total lines minus first 8 and last 8
        result.push_str(&format!("[... {omitted_lines} lines omitted ...]\n"));

        // Display last 8 lines
        for line in lines.iter().skip(lines.len() - 8) {
            result.push_str(line);
            result.push('\n');
        }

        // Pop off final newline, so that we get consistent output regardless of
        // whether, say, files had a final newline or not.
        result.pop();

        result
    }
}

pub(crate) fn ask_user_permission(prefix_message: Option<&str>, message: &str) -> bool {
    use std::io::{self, Write};

    if let Some(prefix) = prefix_message {
        println!("\n{prefix} {message}");
    } else {
        println!("\n{message}");
    }
    print!("Allow this command? (y/n): ");
    io::stdout().flush().unwrap();

    let mut user_input = String::new();
    io::stdin().read_line(&mut user_input).unwrap();
    let user_input = user_input.trim().to_lowercase();

    println!();

    user_input == "y" || user_input == "yes"
}

pub(crate) fn render_markdown(text: &str) {
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
