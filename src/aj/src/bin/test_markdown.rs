use aj::cli::AjCli;
use aj_ui::AjUi;

fn main() {
    let mut ui = AjCli::new(None, false);

    let markdown_text = r#"# Example Markdown Output

This is a **test** of markdown rendering with various elements:

## Code Examples

Here's some Rust code:

```rust
fn example_function() -> Result<String, Box<dyn std::error::Error>> {
    let data = vec![1, 2, 3, 4, 5];
    let result = data.iter()
        .map(|x| x * 2)
        .collect::<Vec<i32>>();
    
    Ok(format!("Processed data: {:?}", result))
}
```

And some Python:

```python
def process_data(items):
    """Process a list of items and return formatted results."""
    results = []
    for item in items:
        if isinstance(item, str):
            results.append(f"String: {item.upper()}")
        elif isinstance(item, int):
            results.append(f"Number: {item * 2}")
    return results
```

## Lists and Structure

### Unordered List
- First item with *italic* text
- Second item with `inline code`
- Third item with [a link](https://example.com)
  - Nested item one
  - Nested item two

### Ordered List
1. **Step one**: Initialize the system
2. **Step two**: Configure parameters
3. **Step three**: Execute the process

## Tables

| Feature | Status | Priority |
|---------|--------|----------|
| Authentication | ✅ Complete | High |
| User Management | 🚧 In Progress | Medium |
| API Documentation | ❌ Pending | Low |

## Blockquotes and Emphasis

> This is a blockquote with some important information.
> 
> It can span multiple lines and contain **bold** and *italic* text.

## Technical Details

- **Language**: Rust 🦀
- **Framework**: Custom agent framework
- **Purpose**: Testing markdown rendering in CLI output
- **Status**: ~Working~ → **Complete**
- **Status**: ~~Crossed-out~~ → **Complete**

---

*This concludes the markdown test content.*"#;

    println!("Testing agent_text_stop with markdown content:\n");
    ui.agent_text_stop(markdown_text);
}
