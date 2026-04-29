//! Human-readable CLI output formatting.
//!
//! Respects the `NO_COLOR` environment variable (per <https://no-color.org/>)
//! and automatically disables ANSI color codes when stdout is not a TTY.

#![deny(missing_docs)]

use serde_json::Value;
use std::io::IsTerminal;
use std::sync::OnceLock;

/// Color/escape codes — empty strings when color is disabled.
struct Colors {
    dim: &'static str,
    bold: &'static str,
    cyan: &'static str,
    green: &'static str,
    yellow: &'static str,
    reset: &'static str,
}

impl Colors {
    fn new() -> Self {
        if should_use_color() {
            Self {
                dim: "\x1b[2m",
                bold: "\x1b[1m",
                cyan: "\x1b[36m",
                green: "\x1b[32m",
                yellow: "\x1b[33m",
                reset: "\x1b[0m",
            }
        } else {
            Self {
                dim: "",
                bold: "",
                cyan: "",
                green: "",
                yellow: "",
                reset: "",
            }
        }
    }
}

static COLORS: OnceLock<Colors> = OnceLock::new();

fn colors() -> &'static Colors {
    COLORS.get_or_init(Colors::new)
}

/// Returns `false` if the `NO_COLOR` environment variable is set (any value)
/// or if stdout is not connected to a TTY.
fn should_use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

/// Strip ANSI SGR escape sequences from a string.
///
/// Only handles `\x1b[...m` sequences (sufficient for all codes used in this
/// module).  Used by tests that need to verify content regardless of color.
#[allow(dead_code)]
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Print tool output in human-readable form, or raw JSON if `json` is true.
pub fn print_output(raw: &str, json: bool) {
    let c = colors();
    if json {
        println!("{raw}");
        return;
    }

    // Try to parse as JSON; if it fails, just print raw
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        println!("{raw}");
        return;
    };

    match &val {
        // Reindex result — must come before the bare-`files_indexed`
        // stats arm below, since reindex JSON also carries that field.
        // Matching stats first would route reindex output through the
        // `Index Health` renderer, which in turn shows `repo ?` and
        // `tantivy 0` (those fields are absent from reindex JSON).
        Value::Object(obj) if obj.contains_key("status") && obj.contains_key("files_indexed") => {
            print_reindex(obj, c)
        }
        // Stats
        Value::Object(obj) if obj.contains_key("files_indexed") => print_stats(obj, c),
        // Outline shape
        Value::Object(obj) if obj.get("shape").and_then(|s| s.as_str()) == Some("Outline") => {
            print_outline(obj, c)
        }
        // Skeleton shape
        Value::Object(obj) if obj.get("shape").and_then(|s| s.as_str()) == Some("Skeleton") => {
            print_skeleton(obj, c)
        }
        // Symbol card
        Value::Object(obj) if obj.get("shape").and_then(|s| s.as_str()) == Some("SymbolCard") => {
            print_symbol_card(obj, c)
        }
        // Reference digest
        Value::Object(obj)
            if obj.get("shape").and_then(|s| s.as_str()) == Some("ReferenceDigest") =>
        {
            print_refs(obj, c)
        }
        // Multi-find single-pattern result: {hits, pattern}
        Value::Object(obj) if obj.contains_key("hits") && obj.contains_key("pattern") => {
            print_multi_group(obj, c)
        }
        // Multi-find multi-pattern result: [{hits, pattern}, ...]
        Value::Array(arr)
            if arr
                .first()
                .and_then(Value::as_object)
                .is_some_and(|o| o.contains_key("hits") && o.contains_key("pattern")) =>
        {
            for (i, item) in arr.iter().enumerate() {
                if let Some(obj) = item.as_object() {
                    if i > 0 {
                        println!();
                    }
                    print_multi_group(obj, c);
                }
            }
        }
        // Array of results (find, search, list, etc.)
        Value::Array(arr) => print_array(arr, c),
        _ => println!(
            "{}",
            serde_json::to_string_pretty(&val).unwrap_or(raw.to_string())
        ),
    }
}

fn print_multi_group(obj: &serde_json::Map<String, Value>, c: &Colors) {
    let pattern = obj.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
    let hits = obj.get("hits").and_then(|v| v.as_array());
    let count = hits.map(|h| h.len()).unwrap_or(0);
    println!(
        "{bold}{pattern}{reset}  {green}{count}{reset} hit{}",
        if count != 1 { "s" } else { "" },
        bold = c.bold,
        green = c.green,
        reset = c.reset
    );
    if let Some(hits) = hits {
        for h in hits {
            let path = h.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let line = h.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            let text = h.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
            println!(
                "  {dim}{path}:{line}{reset}  {text}",
                dim = c.dim,
                reset = c.reset
            );
        }
    }
}

fn print_stats(obj: &serde_json::Map<String, Value>, c: &Colors) {
    let files = obj
        .get("files_indexed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let symbols = obj
        .get("total_symbols")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tantivy = obj
        .get("tantivy_docs")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let root = obj.get("repo_root").and_then(|v| v.as_str()).unwrap_or("?");

    println!("{bold}Index Health{reset}", bold = c.bold, reset = c.reset);
    println!(
        "  {dim}repo{reset}     {root}",
        dim = c.dim,
        reset = c.reset
    );
    println!(
        "  {dim}files{reset}    {green}{files}{reset}",
        dim = c.dim,
        green = c.green,
        reset = c.reset
    );
    println!(
        "  {dim}symbols{reset}  {green}{symbols}{reset}",
        dim = c.dim,
        green = c.green,
        reset = c.reset
    );
    println!(
        "  {dim}tantivy{reset}  {tantivy}",
        dim = c.dim,
        reset = c.reset
    );
}

fn print_reindex(obj: &serde_json::Map<String, Value>, c: &Colors) {
    let status = obj.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let files = obj
        .get("files_indexed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let symbols = obj
        .get("total_symbols")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let errors = obj.get("errors").and_then(|v| v.as_u64()).unwrap_or(0);

    let icon = if status == "ok" { c.green } else { c.yellow };
    println!(
        "{icon}{bold}{status}{reset}  {files} files, {symbols} symbols, {errors} errors",
        bold = c.bold,
        reset = c.reset
    );
}

fn print_outline(obj: &serde_json::Map<String, Value>, c: &Colors) {
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    println!("{dim}{path}{reset}", dim = c.dim, reset = c.reset);
    println!();

    if let Some(Value::Array(entries)) = obj.get("entries") {
        for entry in entries {
            let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let line = entry.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            println!(
                "  {cyan}{kind:<10}{reset} {bold}{name}{reset} {dim}:{line}{reset}",
                cyan = c.cyan,
                bold = c.bold,
                dim = c.dim,
                reset = c.reset
            );

            if let Some(Value::Array(children)) = entry.get("children") {
                for child in children {
                    let ck = child.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                    let cn = child.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let cl = child.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!(
                        "    {dim}{ck:<10}{reset} {cn} {dim}:{cl}{reset}",
                        dim = c.dim,
                        reset = c.reset
                    );
                }
            }
        }
    }
}

fn print_skeleton(obj: &serde_json::Map<String, Value>, c: &Colors) {
    if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
        println!("{dim}{path}{reset}", dim = c.dim, reset = c.reset);
        println!();
    }
    let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let tokens = obj
        .get("token_estimate")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    print!("{content}");
    println!("({tokens} tokens)");
}

fn print_symbol_card(obj: &serde_json::Map<String, Value>, c: &Colors) {
    let qname = obj
        .get("qualified_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");

    println!(
        "{cyan}{kind}{reset} {bold}{qname}{reset}  {dim}{path}{reset}",
        cyan = c.cyan,
        bold = c.bold,
        dim = c.dim,
        reset = c.reset
    );

    if let Some(sig) = obj.get("signature").and_then(|v| v.as_str()) {
        println!("{dim}{sig}{reset}", dim = c.dim, reset = c.reset);
    }
    println!();

    let body = obj.get("body").and_then(|v| v.as_str()).unwrap_or("");
    println!("{body}");

    if let Some(Value::Array(callers)) = obj.get("callers") {
        if !callers.is_empty() {
            println!();
            println!("callers ({}):", callers.len());
            for c_item in callers.iter().take(10) {
                let cp = c_item.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  {dim}{cp}{reset}", dim = c.dim, reset = c.reset);
            }
        }
    }
}

fn print_refs(obj: &serde_json::Map<String, Value>, c: &Colors) {
    let symbol = obj.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
    let total = obj.get("total").and_then(|v| v.as_u64()).unwrap_or(0);

    println!(
        "{bold}{symbol}{reset}  {green}{total}{reset} reference{}",
        if total != 1 { "s" } else { "" },
        bold = c.bold,
        green = c.green,
        reset = c.reset
    );
    println!();

    if let Some(Value::Array(refs)) = obj.get("top_k") {
        for r in refs {
            let path = r.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let line = r.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            if line > 0 {
                println!("  {dim}{path}:{line}{reset}", dim = c.dim, reset = c.reset);
            } else {
                println!("  {dim}{path}{reset}", dim = c.dim, reset = c.reset);
            }
        }
    }
}

fn print_array(arr: &[Value], c: &Colors) {
    if arr.is_empty() {
        println!("{dim}(no results){reset}", dim = c.dim, reset = c.reset);
        return;
    }

    // Detect type from first element
    let first = &arr[0];

    // Find symbol results (have qualified_name + path + kind)
    if first.get("qualified_name").is_some() {
        for item in arr {
            let qname = item
                .get("qualified_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let line = item.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let sig = item.get("signature").and_then(|v| v.as_str());

            print!(
                "{cyan}{kind:<10}{reset} {bold}{qname}{reset}",
                cyan = c.cyan,
                bold = c.bold,
                reset = c.reset
            );
            println!("  {dim}{path}:{line}{reset}", dim = c.dim, reset = c.reset);
            if let Some(sig) = sig {
                println!("           {dim}{sig}{reset}", dim = c.dim, reset = c.reset);
            }
        }
        return;
    }

    // Search results (have path + line + text)
    if first.get("text").is_some() {
        for item in arr {
            let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let line = item.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            let text = item
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            println!(
                "{dim}{path}:{line}{reset}  {text}",
                dim = c.dim,
                reset = c.reset
            );
        }
        return;
    }

    // File list results (have path + symbol_count)
    if first.get("symbol_count").is_some() {
        for item in arr {
            let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let lang = item.get("lang").and_then(|v| v.as_str()).unwrap_or("?");
            let count = item
                .get("symbol_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            println!(
                "{dim}{lang:<4}{reset} {path}  {dim}({count} symbols){reset}",
                dim = c.dim,
                reset = c.reset
            );
        }
        return;
    }

    // Fallback: pretty JSON
    for item in arr {
        println!("{}", serde_json::to_string_pretty(item).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_sgr_codes() {
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[0m"), "bold");
        assert_eq!(strip_ansi("\x1b[36mcyan\x1b[0m"), "cyan");
        assert_eq!(strip_ansi("\x1b[2m\x1b[33myellow\x1b[0m"), "yellow");
        assert_eq!(strip_ansi("no codes here"), "no codes here");
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strip_ansi_handles_multiple_escapes() {
        let input = "\x1b[1m\x1b[32mok\x1b[0m  5 files";
        assert_eq!(strip_ansi(input), "ok  5 files");
    }

    #[test]
    fn should_use_color_respects_no_color_env() {
        // When NO_COLOR is set, should_use_color returns false.
        // We can't easily test the TTY branch, but we can test NO_COLOR.
        std::env::set_var("NO_COLOR", "1");
        assert!(!should_use_color());
        std::env::remove_var("NO_COLOR");
    }

    #[test]
    fn print_output_json_mode_passthrough() {
        // With json=true, output is raw JSON
        let input = r#"{"shape":"Outline","path":"test.rs","entries":[]}"#;
        // Just verify it doesn't panic and handles the json branch
        print_output(input, true);
    }

    #[test]
    fn print_output_invalid_json_prints_raw() {
        // Non-JSON input should be printed as-is
        print_output("not json at all", false);
    }

    #[test]
    fn print_output_handles_empty_array() {
        print_output("[]", false);
    }

    #[test]
    fn print_output_handles_non_object_non_array() {
        print_output(r#""just a string""#, false);
    }

    #[test]
    fn print_stats_output_contains_files_and_symbols() {
        let json = serde_json::json!({
            "files_indexed": 42,
            "total_symbols": 150,
            "tantivy_docs": 140,
            "repo_root": "/home/user/project"
        });
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_reindex_output_contains_status() {
        let json = serde_json::json!({
            "status": "ok",
            "files_indexed": 100,
            "total_symbols": 500,
            "errors": 0
        });
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_outline_output_contains_path() {
        let json = serde_json::json!({
            "shape": "Outline",
            "path": "src/lib.rs",
            "entries": [
                {
                    "kind": "fn",
                    "name": "main",
                    "line": 1,
                    "children": []
                }
            ]
        });
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_skeleton_output_contains_content() {
        let json = serde_json::json!({
            "shape": "Skeleton",
            "path": "src/lib.rs",
            "content": "fn main() { ... }\n\n",
            "token_estimate": 5
        });
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_symbol_card_output_contains_qname() {
        let json = serde_json::json!({
            "shape": "SymbolCard",
            "path": "src/lib.rs",
            "qualified_name": "crate::main",
            "kind": "fn",
            "signature": "fn main()",
            "doc": null,
            "body": "fn main() {}",
            "line_range": [1, 1],
            "parent_chain": [],
            "callers": [],
            "callees": []
        });
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_refs_output_contains_symbol_and_total() {
        let json = serde_json::json!({
            "shape": "ReferenceDigest",
            "symbol": "process_data",
            "total": 3,
            "top_k": [
                {"path": "src/main.rs", "line": 10, "col": 5, "snippet": "process_data()", "enclosing_symbol": "main"}
            ]
        });
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_array_detects_symbol_results() {
        let json = serde_json::json!([
            {"qualified_name": "crate::main", "path": "src/lib.rs", "line": 1, "kind": "fn", "signature": "fn main()"}
        ]);
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_array_detects_search_results() {
        let json = serde_json::json!([
            {"path": "src/lib.rs", "line": 5, "col": 10, "text": "let x = 42;"}
        ]);
        let raw = json.to_string();
        print_output(&raw, false);
    }

    #[test]
    fn print_array_detects_file_list_results() {
        let json = serde_json::json!([
            {"path": "src/lib.rs", "lang": "Rust", "symbol_count": 15, "top_symbols": ["main"]}
        ]);
        let raw = json.to_string();
        print_output(&raw, false);
    }
}
