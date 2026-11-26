//! The tools Claude may call in a LingOS session, and what each needs from the user.
//!
//! Reading (read_file, list_dir, search) runs at once. Anything that changes state asks
//! first: a shell command (lingos_shell), a file write or edit. The user answers
//! y (this once), n [reason] (refused; the reason goes back to Claude), e (accept every
//! file edit under the session's project folder for the rest of the session), or, for
//! a read-only shell command, a (allow that command for the session).
//!
//! Paths are the SSH shell's: `/media/lingdata/...` absolute, `~/...` the user's home,
//! anything else relative to the session's working directory. The system's account
//! stores are refused (vfs::is_account_secret), as in the SSH shell.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::{json, Value};

/// Shell commands that only look (an "a" answer may allow them for the session).
pub const READ_ONLY: &[&str] = &[
    "ls", "dir", "cat", "pwd", "stat", "sha256sum", "df", "info", "sysinfo", "running", "uptime", "free", "date", "hostname", "uname", "whoami", "ps", "apps", "models", "net", "board",
    "help", "cd", "hitches", "audio", "gpu", "cpu", "hwinfo", "specs",
];

/// The tool definitions (strict schemas; eager input streaming -- inputs are validated here).
pub fn definitions() -> Value {
    let obj = |props: Value, req: &[&str]| json!({"type": "object", "properties": props, "required": req, "additionalProperties": false});
    json!([
        {
            "name": "lingos_shell",
            "description": "Run one command in the LingOS shell (the SSH command set: ls, cat, cd, cp, mv, rm, mkdir, stat, sha256sum, df, info, running, ling run <file.ling>, open <app>, and more; `help` lists them). Runs in the session's working directory; `cd` changes it for later calls. Use it to inspect the machine, run a Ling program or a LingDev project, or manage files when the file tools do not fit. Every call is shown to the user for approval first.",
            "strict": true,
            "eager_input_streaming": true,
            "input_schema": obj(json!({"command": {"type": "string", "description": "The command line, e.g. `ling run main.ling` or `ls -l assets`"}}), &["command"])
        },
        {
            "name": "list_dir",
            "description": "List a folder (files with sizes, folders with a trailing /). Use it first to learn a project's layout. With recursive true it walks sub-folders breadth-first up to max_entries.",
            "strict": true,
            "eager_input_streaming": true,
            "input_schema": obj(json!({
                "path": {"type": "string", "description": "Folder; \".\" is the working directory"},
                "recursive": {"type": "boolean"},
                "max_entries": {"type": "integer", "description": "Default 300, at most 2000"}
            }), &["path", "recursive", "max_entries"])
        },
        {
            "name": "search",
            "description": "Find text in the files under a folder (a literal substring, optionally case-insensitive), like grep -rn. Returns file:line: text. Use it to find where something is defined or used before editing.",
            "strict": true,
            "eager_input_streaming": true,
            "input_schema": obj(json!({
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Folder or file; \".\" is the working directory"},
                "ignore_case": {"type": "boolean"},
                "max_results": {"type": "integer", "description": "Default 100, at most 1000"}
            }), &["pattern", "path", "ignore_case", "max_results"])
        },
        {
            "name": "read_file",
            "description": "Read a text file with line numbers. offset is the first line (1-based) and limit the number of lines; read long files in parts.",
            "strict": true,
            "eager_input_streaming": true,
            "input_schema": obj(json!({
                "path": {"type": "string"},
                "offset": {"type": "integer", "description": "First line, 1-based (1 = the start)"},
                "limit": {"type": "integer", "description": "Lines to read (default 400, at most 2000)"}
            }), &["path", "offset", "limit"])
        },
        {
            "name": "edit_file",
            "description": "Replace an exact piece of text in a file. old_string must occur exactly once unless replace_all is true; include enough surrounding lines to make it unique. Read the file first. The user approves each edit (or all edits in the project).",
            "strict": true,
            "eager_input_streaming": true,
            "input_schema": obj(json!({
                "path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            }), &["path", "old_string", "new_string", "replace_all"])
        },
        {
            "name": "write_file",
            "description": "Create a file or replace its whole contents. Prefer edit_file for changes to an existing file. The user approves each write (or all edits in the project).",
            "strict": true,
            "eager_input_streaming": true,
            "input_schema": obj(json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"])
        }
    ])
}

/// The SSH shell's path rules (sshcmds::vpath): "/x" absolute, "~" home, else cwd-relative.
pub fn resolve(cwd: &str, user: &str, p: &str) -> String {
    let p = p.trim();
    if let Some(abs) = p.strip_prefix('/') {
        crate::vfs::norm(abs)
    } else if p == "~" {
        format!("home/{user}")
    } else if let Some(rest) = p.strip_prefix("~/") {
        crate::vfs::join(&format!("home/{user}"), rest)
    } else if p == "." || p.is_empty() {
        String::from(cwd)
    } else {
        crate::vfs::join(cwd, p)
    }
}

/// May this user touch `path`? Not the account stores, not another user's home.
fn allowed(user: &str, path: &str) -> Result<(), String> {
    if crate::vfs::is_account_secret(path) {
        return Err(String::from("permission denied: that is the system's account data"));
    }
    if let Some(rest) = path.strip_prefix("home/") {
        let owner = rest.split('/').next().unwrap_or("");
        if !owner.is_empty() && owner != user && user != "root" {
            return Err(format!("permission denied: /home/{owner} belongs to another user"));
        }
    }
    Ok(())
}

/// A call as the user sees it, and whether it needs approval / which kind.
pub enum Need {
    None,
    /// (what to show, the command name for an "a" answer if read-only)
    Shell(String, Option<String>),
    /// (what to show, the resolved path)
    Edit(String, String),
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

/// Check a tool call's input against its schema (eager streaming does not): Err = INVALID input.
pub fn validate(name: &str, inp: &Value) -> Result<(), String> {
    let o = inp.as_object().ok_or("input is not an object")?;
    let need: &[(&str, &str)] = match name {
        "lingos_shell" => &[("command", "s")],
        "list_dir" => &[("path", "s"), ("recursive", "b"), ("max_entries", "i")],
        "search" => &[("pattern", "s"), ("path", "s"), ("ignore_case", "b"), ("max_results", "i")],
        "read_file" => &[("path", "s"), ("offset", "i"), ("limit", "i")],
        "edit_file" => &[("path", "s"), ("old_string", "s"), ("new_string", "s"), ("replace_all", "b")],
        "write_file" => &[("path", "s"), ("content", "s")],
        _ => return Err(format!("no tool named {name}")),
    };
    for (k, t) in need {
        let v = o.get(*k).ok_or_else(|| format!("missing {k}"))?;
        let ok = match *t {
            "s" => v.is_string(),
            "b" => v.is_boolean(),
            _ => v.is_i64() || v.is_u64(),
        };
        if !ok {
            return Err(format!("{k} has the wrong type"));
        }
    }
    if o.len() != need.len() {
        return Err(String::from("unexpected fields"));
    }
    Ok(())
}

/// What approval a (valid) call needs.
pub fn need(name: &str, inp: &Value, cwd: &str, user: &str) -> Need {
    match name {
        "lingos_shell" => {
            let c = s(inp, "command").unwrap_or("").trim().to_string();
            let first = c.split_whitespace().next().unwrap_or("").to_string();
            let ro = READ_ONLY.contains(&first.as_str());
            Need::Shell(c, ro.then_some(first))
        },
        "edit_file" => {
            let p = resolve(cwd, user, s(inp, "path").unwrap_or(""));
            Need::Edit(format!("edit /{p}"), p)
        },
        "write_file" => {
            let p = resolve(cwd, user, s(inp, "path").unwrap_or(""));
            let exists = crate::vfs::exists(&p);
            Need::Edit(format!("{} /{p} ({} bytes)", if exists { "overwrite" } else { "create" }, s(inp, "content").unwrap_or("").len()), p)
        },
        _ => Need::None,
    }
}

/// A short diff of an edit for the approval prompt: (removed lines, added lines), each capped.
pub fn edit_preview(inp: &Value) -> (Vec<String>, Vec<String>) {
    let cap = |t: &str| -> Vec<String> {
        let mut v: Vec<String> = t.lines().take(12).map(|l| l.chars().take(160).collect()).collect();
        let n = t.lines().count();
        if n > 12 {
            v.push(format!("... ({} more lines)", n - 12));
        }
        v
    };
    match (s(inp, "old_string"), s(inp, "new_string"), s(inp, "content")) {
        (Some(o), Some(n), _) => (cap(o), cap(n)),
        (_, _, Some(c)) => (Vec::new(), cap(c)),
        _ => (Vec::new(), Vec::new()),
    }
}

const MAX_RESULT: usize = 40 * 1024;

fn clip(mut t: String) -> String {
    if t.len() > MAX_RESULT {
        let mut cut = MAX_RESULT;
        while !t.is_char_boundary(cut) {
            cut -= 1;
        }
        t.truncate(cut);
        t.push_str(&format!("\n[... output cut at {} KB; ask for less, e.g. a line range or a narrower search]", MAX_RESULT / 1024));
    }
    t
}

fn is_text(b: &[u8]) -> bool {
    // no NUL byte near the start (binary files have them; UTF-8 text never does)
    !b.iter().take(4096).any(|&c| c == 0)
}

/// Run an approved / read-only call: Ok(result text) or Err(error text, sent with is_error).
pub fn run(name: &str, inp: &Value, cwd: &mut String, user: &str) -> Result<String, String> {
    match name {
        "lingos_shell" => {
            let line = s(inp, "command").unwrap_or("").trim();
            let (cmd, arg) = match line.find(' ') {
                Some(i) => (&line[..i], line[i + 1..].trim()),
                None => (line, ""),
            };
            if cmd == "claude" {
                return Err(String::from("claude cannot be started from inside a claude session"));
            }
            match crate::drivers::sshcmds::run(cmd, arg, user, cwd) {
                Some(out) => Ok(clip(if out.trim().is_empty() { String::from("(no output)") } else { out })),
                None => Err(format!("{cmd}: not a LingOS shell command (`help` lists them)")),
            }
        },
        "list_dir" => {
            let root = resolve(cwd, user, s(inp, "path").unwrap_or("."));
            allowed(user, &root)?;
            if !root.is_empty() && !crate::vfs::is_dir(&root) {
                return Err(format!("/{root} is not a folder"));
            }
            let max = inp.get("max_entries").and_then(|x| x.as_u64()).unwrap_or(300).clamp(1, 2000) as usize;
            let rec = inp.get("recursive").and_then(|x| x.as_bool()).unwrap_or(false);
            let mut out = String::new();
            let mut queue = alloc::collections::VecDeque::new();
            queue.push_back(root.clone());
            let mut n = 0usize;
            while let Some(dir) = queue.pop_front() {
                let mut items = crate::vfs::list(&dir);
                items.sort_by(|a, b| a.0.cmp(&b.0));
                for (name, is_dir, size) in items {
                    if n >= max {
                        out.push_str(&format!("[... stopped at {max} entries]\n"));
                        return Ok(out);
                    }
                    let full = crate::vfs::join(&dir, &name);
                    let rel = full.strip_prefix(&root).map(|r| r.trim_start_matches('/')).unwrap_or(&full);
                    if is_dir {
                        out.push_str(&format!("{rel}/\n"));
                        if rec && !name.starts_with('.') {
                            queue.push_back(full.clone());
                        }
                    } else {
                        out.push_str(&format!("{rel}  {size}\n"));
                    }
                    n += 1;
                }
            }
            Ok(if out.is_empty() { String::from("(empty)") } else { out })
        },
        "search" => {
            let pat = s(inp, "pattern").unwrap_or("");
            if pat.is_empty() {
                return Err(String::from("an empty pattern"));
            }
            let ic = inp.get("ignore_case").and_then(|x| x.as_bool()).unwrap_or(false);
            let max = inp.get("max_results").and_then(|x| x.as_u64()).unwrap_or(100).clamp(1, 1000) as usize;
            let root = resolve(cwd, user, s(inp, "path").unwrap_or("."));
            allowed(user, &root)?;
            let needle = if ic { pat.to_lowercase() } else { String::from(pat) };
            let mut out = String::new();
            let mut hits = 0usize;
            let mut files = 0usize;
            let mut stack = Vec::new();
            stack.push(root.clone());
            while let Some(p) = stack.pop() {
                if crate::vfs::is_dir(&p) {
                    for (name, _, _) in crate::vfs::list(&p) {
                        if !name.starts_with('.') {
                            stack.push(crate::vfs::join(&p, &name));
                        }
                    }
                    continue;
                }
                files += 1;
                if files > 5000 {
                    out.push_str("[... stopped after 5000 files; search a narrower folder]\n");
                    break;
                }
                let Some(bytes) = crate::vfs::read(&p) else { continue };
                if bytes.len() > 4 << 20 || !is_text(&bytes) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes);
                for (ln, line) in text.lines().enumerate() {
                    let hay = if ic { line.to_lowercase() } else { String::from(line) };
                    if hay.contains(&needle) {
                        out.push_str(&format!("/{p}:{}: {}\n", ln + 1, line.trim().chars().take(200).collect::<String>()));
                        hits += 1;
                        if hits >= max {
                            out.push_str(&format!("[... stopped at {max} results]\n"));
                            return Ok(clip(out));
                        }
                    }
                }
                crate::coro::maybe_preempt();
            }
            Ok(if out.is_empty() { format!("no matches for {pat:?}") } else { clip(out) })
        },
        "read_file" => {
            let p = resolve(cwd, user, s(inp, "path").unwrap_or(""));
            allowed(user, &p)?;
            let bytes = crate::vfs::read(&p).ok_or_else(|| format!("/{p}: no such file"))?;
            if !is_text(&bytes) {
                return Err(format!("/{p} is not a text file ({} bytes)", bytes.len()));
            }
            let text = String::from_utf8_lossy(&bytes);
            let total = text.lines().count();
            let off = inp.get("offset").and_then(|x| x.as_u64()).unwrap_or(1).max(1) as usize;
            let lim = inp.get("limit").and_then(|x| x.as_u64()).unwrap_or(400).clamp(1, 2000) as usize;
            let mut out = String::new();
            for (i, l) in text.lines().enumerate().skip(off - 1).take(lim) {
                out.push_str(&format!("{:>5}\t{}\n", i + 1, l));
            }
            if off - 1 + lim < total {
                out.push_str(&format!("[lines {off}-{} of {total}; read on with offset {}]\n", off - 1 + lim, off + lim));
            }
            Ok(clip(if out.is_empty() { format!("(the file has {total} lines)") } else { out }))
        },
        "edit_file" => {
            let p = resolve(cwd, user, s(inp, "path").unwrap_or(""));
            allowed(user, &p)?;
            let old = s(inp, "old_string").unwrap_or("");
            let new = s(inp, "new_string").unwrap_or("");
            let all = inp.get("replace_all").and_then(|x| x.as_bool()).unwrap_or(false);
            if old.is_empty() {
                return Err(String::from("old_string is empty (use write_file to create a file)"));
            }
            let bytes = crate::vfs::read(&p).ok_or_else(|| format!("/{p}: no such file"))?;
            let text = String::from_utf8(bytes).map_err(|_| format!("/{p} is not UTF-8 text"))?;
            let count = text.matches(old).count();
            if count == 0 {
                return Err(String::from("old_string was not found (read the file again: it must match exactly, spaces included)"));
            }
            if count > 1 && !all {
                return Err(format!("old_string occurs {count} times; add surrounding lines to make it unique, or set replace_all"));
            }
            let out = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
            if !crate::vfs::write(&p, out.as_bytes()) {
                return Err(format!("could not write /{p}"));
            }
            Ok(format!("edited /{p} ({} replacement{})", if all { count } else { 1 }, if all && count != 1 { "s" } else { "" }))
        },
        "write_file" => {
            let p = resolve(cwd, user, s(inp, "path").unwrap_or(""));
            allowed(user, &p)?;
            let c = s(inp, "content").unwrap_or("");
            let parent = crate::vfs::parent(&p);
            if !parent.is_empty() && !crate::vfs::is_dir(&parent) {
                let _ = crate::vfs::mkdir(&parent);
            }
            if !crate::vfs::write(&p, c.as_bytes()) {
                return Err(format!("could not write /{p}"));
            }
            Ok(format!("wrote /{p} ({} bytes, {} lines)", c.len(), c.lines().count()))
        },
        _ => Err(format!("no tool named {name}")),
    }
}
