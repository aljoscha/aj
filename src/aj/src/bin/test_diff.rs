use aj::ui_cli::AjCli;
use aj_ui::AjUi;

fn main() {
    let ui = AjCli;

    let tool_name = "edit_file";
    let input = r#"{"path": "/path/to/file.rs", "old_string": "fn old_function()", "new_string": "fn new_function()"}"#;

    let before = r#"use std::collections::HashMap;

pub struct Example {
    data: HashMap<String, i32>,
    count: usize,
}

impl Example {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
            count: 0,
        }
    }
    
    fn old_function(&self) -> bool {
        self.count > 10
    }
    
    pub fn add_item(&mut self, key: String, value: i32) {
        self.data.insert(key, value);
        self.count += 1;
    }
    
    pub fn get_count(&self) -> usize {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_new() {
        let example = Example::new();
        assert_eq!(example.count, 0);
    }
}"#;

    let after = r#"use std::collections::HashMap;

pub struct Example {
    data: HashMap<String, i32>,
    count: usize,
}

impl Example {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
            count: 0,
        }
    }
    
    fn new_function(&self) -> bool {
        self.count > 10
    }
    
    pub fn add_item(&mut self, key: String, value: i32) {
        self.data.insert(key, value);
        self.count += 1;
    }
    
    pub fn get_count(&self) -> usize {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_new() {
        let example = Example::new();
        assert_eq!(example.count, 0);
    }
    
    #[test]
    fn test_add_item() {
        let mut example = Example::new();
        example.add_item("test".to_string(), 42);
        assert_eq!(example.count, 1);
    }
}"#;

    println!("Testing display_tool_result_diff with example data:\n");
    ui.display_tool_result_diff(tool_name, input, before, after);
}
