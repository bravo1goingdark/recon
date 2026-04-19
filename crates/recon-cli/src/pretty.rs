//! Human-readable CLI output formatting.

use serde_json::Value;

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Print tool output in human-readable form, or raw JSON if `json` is true.
pub fn print_output(raw: &str, json: bool) {
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
        // Stats
        Value::Object(obj) if obj.contains_key("files_indexed") => print_stats(obj),
        // Outline shape
        Value::Object(obj) if obj.get("shape").and_then(|s| s.as_str()) == Some("Outline") => {
            print_outline(obj)
        }
        // Skeleton shape
        Value::Object(obj) if obj.get("shape").and_then(|s| s.as_str()) == Some("Skeleton") => {
            print_skeleton(obj)
        }
        // Symbol card
        Value::Object(obj) if obj.get("shape").and_then(|s| s.as_str()) == Some("SymbolCard") => {
            print_symbol_card(obj)
        }
        // Reference digest
        Value::Object(obj)
            if obj.get("shape").and_then(|s| s.as_str()) == Some("ReferenceDigest") =>
        {
            print_refs(obj)
        }
        // Reindex result
        Value::Object(obj) if obj.contains_key("status") && obj.contains_key("files_indexed") => {
            print_reindex(obj)
        }
        // Array of results (find, search, list, etc.)
        Value::Array(arr) => print_array(arr),
        _ => println!(
            "{}",
            serde_json::to_string_pretty(&val).unwrap_or(raw.to_string())
        ),
    }
}

fn print_stats(obj: &serde_json::Map<String, Value>) {
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

    println!("{BOLD}Index Health{RESET}");
    println!("  {DIM}repo{RESET}     {root}");
    println!("  {DIM}files{RESET}    {GREEN}{files}{RESET}");
    println!("  {DIM}symbols{RESET}  {GREEN}{symbols}{RESET}");
    println!("  {DIM}tantivy{RESET}  {tantivy}");
}

fn print_reindex(obj: &serde_json::Map<String, Value>) {
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

    let icon = if status == "ok" { GREEN } else { YELLOW };
    println!("{icon}{BOLD}{status}{RESET}  {files} files, {symbols} symbols, {errors} errors");
}

fn print_outline(obj: &serde_json::Map<String, Value>) {
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    println!("{DIM}{path}{RESET}");
    println!();

    if let Some(Value::Array(entries)) = obj.get("entries") {
        for entry in entries {
            let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let line = entry.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("  {CYAN}{kind:<10}{RESET} {BOLD}{name}{RESET} {DIM}:{line}{RESET}");

            if let Some(Value::Array(children)) = entry.get("children") {
                for child in children {
                    let ck = child.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                    let cn = child.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let cl = child.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                    println!("    {DIM}{ck:<10}{RESET} {cn} {DIM}:{cl}{RESET}");
                }
            }
        }
    }
}

fn print_skeleton(obj: &serde_json::Map<String, Value>) {
    if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
        println!("{DIM}{path}{RESET}");
        println!();
    }
    let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let tokens = obj
        .get("token_estimate")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    print!("{content}");
    println!("{DIM}({tokens} tokens){RESET}");
}

fn print_symbol_card(obj: &serde_json::Map<String, Value>) {
    let qname = obj
        .get("qualified_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");

    println!("{CYAN}{kind}{RESET} {BOLD}{qname}{RESET}  {DIM}{path}{RESET}");

    if let Some(sig) = obj.get("signature").and_then(|v| v.as_str()) {
        println!("{DIM}{sig}{RESET}");
    }
    println!();

    let body = obj.get("body").and_then(|v| v.as_str()).unwrap_or("");
    println!("{body}");

    if let Some(Value::Array(callers)) = obj.get("callers") {
        if !callers.is_empty() {
            println!();
            println!("{DIM}callers ({}):{RESET}", callers.len());
            for c in callers.iter().take(10) {
                let cp = c.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                println!("  {DIM}{cp}{RESET}");
            }
        }
    }
}

fn print_refs(obj: &serde_json::Map<String, Value>) {
    let symbol = obj.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
    let total = obj.get("total").and_then(|v| v.as_u64()).unwrap_or(0);

    println!(
        "{BOLD}{symbol}{RESET}  {GREEN}{total}{RESET} reference{}",
        if total != 1 { "s" } else { "" }
    );
    println!();

    if let Some(Value::Array(refs)) = obj.get("top_k") {
        for r in refs {
            let path = r.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let line = r.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            if line > 0 {
                println!("  {DIM}{path}:{line}{RESET}");
            } else {
                println!("  {DIM}{path}{RESET}");
            }
        }
    }
}

fn print_array(arr: &[Value]) {
    if arr.is_empty() {
        println!("{DIM}(no results){RESET}");
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

            print!("{CYAN}{kind:<10}{RESET} {BOLD}{qname}{RESET}");
            println!("  {DIM}{path}:{line}{RESET}");
            if let Some(sig) = sig {
                println!("           {DIM}{sig}{RESET}");
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
            println!("{DIM}{path}:{line}{RESET}  {text}");
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
            println!("{DIM}{lang:<4}{RESET} {path}  {DIM}({count} symbols){RESET}");
        }
        return;
    }

    // Fallback: pretty JSON
    for item in arr {
        println!("{}", serde_json::to_string_pretty(item).unwrap_or_default());
    }
}
