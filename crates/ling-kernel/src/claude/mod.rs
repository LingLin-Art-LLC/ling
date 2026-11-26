//! `claude`: Claude in the LingOS shell -- over SSH and in the desktop's Terminal.
//!
//! `claude` starts a session in the shell's working directory: the welcome box, then
//! a `>` prompt. Each prompt goes to the Claude API (Claude Opus 5.5, streamed, adaptive
//! thinking, prompt caching, server-side refusal fallback); the reply shows while it is
//! written, and Claude can use tools (tools.rs) -- reading runs at once, anything that
//! changes the machine waits for the user's y / n / e / a. Ctrl+C stops a reply (a second
//! Ctrl+C, /exit or Ctrl+D leaves). Conversations are saved sealed (history.rs) and can be
//! resumed with the folder they were started in.
//!
//! `claude login` takes an API key from the Claude Console without echo and keeps it in
//! the account's sealed credential store as "anthropic" (the same entry Settings > Sign-in
//! and Ling Coder use); `claude logout` erases it; `claude status` says what is set. The
//! key is only ever sent to api.anthropic.com over TLS with its certificate verified
//! (tls.rs + x509.rs); a test server (`claude --base-url http://host:port`) gets a dummy key.
//!
//! Each session is a coroutine (coro.rs) with a little shared state; the SSH session
//! (sshd.rs) and the Terminal (drivers/terminal.rs) are pollers: they hand it typed lines
//! (`input_line`), Ctrl+C (`interrupt`) and take what it printed (`take_ansi` / `take_segs`).

pub mod api;
pub mod history;
pub mod style;
pub mod tools;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::{json, Value};
use style::{Colors, Role};

pub const MODEL: &str = "claude-opus-5-5";
pub const MODEL_NAME: &str = "Claude Opus 5.5";
const CRED: &str = "anthropic";
/// The Terminal window's session id (SSH sessions use their connection handle + 1).
pub const TERM_ID: u64 = u64::MAX;

const SYSTEM: &str = "You are Claude, running inside LingOS -- a hobby operating system written from scratch (a Rust kernel; apps written in the Ling language). The user talks to you from the LingOS shell (over SSH or the desktop Terminal). \
You can act on the machine through tools: lingos_shell runs one LingOS shell command (the SSH command set; `help` lists it) in the session's working directory; list_dir, search and read_file look at files; edit_file and write_file change them. \
Commands and file changes are shown to the user and wait for approval; if the user declines, take that into account rather than retrying the same thing. \
Paths look like the shell shows them: /media/lingdata/... for the data drive, ~ for the user's home, others relative to the working directory. \
LingDev projects are games written in Ling: a main .ling file plus assets (models, textures, sounds) in sub-folders; `ling run <file.ling>` runs a program. Before changing a project, look at its layout and read the files you will touch; keep the project's existing style. \
Keep replies concise and plain-text friendly (the terminal shows no Markdown rendering).";

#[derive(Clone, Copy, PartialEq)]
enum St {
    Idle,
    Busy,
    Ask,
    Key,
    Ended,
}

struct Sess {
    id: u64,
    user: String,
    cwd: String,
    colors: Colors,
    out: Vec<(Role, String)>,
    at_bol: bool,
    inq: VecDeque<String>,
    answer: Option<String>,
    state: St,
    cancel: bool,
    task: u64,
    messages: Vec<Value>,
    conv: String,
    title: String,
    started_cwd: String,
    effort: String,
    target: api::Target,
    allow_cmds: Vec<String>,
    edits_under: Option<String>,
    tokens: u64,
    cost: f64,
    /// SSH: the line being typed (the Terminal edits its own)
    line: Vec<u8>,
}

static mut SESSIONS: Vec<Box<Sess>> = Vec::new();

fn sessions() -> &'static mut Vec<Box<Sess>> {
    unsafe { &mut *(&raw mut SESSIONS) }
}

/// Run `f` on session `id` (never hold the reference across a yield).
fn with<R>(id: u64, f: impl FnOnce(&mut Sess) -> R) -> Option<R> {
    sessions().iter_mut().find(|s| s.id == id).map(|s| f(s))
}

fn say(id: u64, role: Role, text: &str) {
    with(id, |s| {
        if text.is_empty() {
            return;
        }
        s.at_bol = text.ends_with('\n');
        s.out.push((role, String::from(text)));
    });
}

/// Start a new line if the last output did not end one.
fn nl(id: u64) {
    with(id, |s| {
        if !s.at_bol {
            s.out.push((Role::Text, String::from("\n")));
            s.at_bol = true;
        }
    });
}

fn shown_cwd(cwd: &str) -> String {
    format!("/{cwd}")
}

// ── the API key ──

fn key_for(user: &str) -> Result<String, String> {
    if crate::keyring::user().as_deref() != Some(user) || !crate::keyring::is_unlocked() {
        return Err(format!("the key is kept in {user}'s sealed store: sign in on the LingOS desktop as {user} first"));
    }
    crate::creds::kernel_get(CRED).ok_or_else(|| String::from("no API key yet: run `claude login` (get one at console.anthropic.com > API Keys)"))
}

fn plausible_key(k: &str) -> bool {
    k.starts_with("sk-ant-") && (24..=400).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_graphic())
}

fn status_text(user: &str) -> String {
    let key = match key_for(user) {
        Ok(k) => format!("API key set (sk-ant-...{})", &k[k.len().saturating_sub(4)..]),
        Err(e) => e,
    };
    let n = history::list(user).len();
    format!("claude: {key}\nmodel {MODEL_NAME} ({MODEL}); {n} saved conversation(s); key sent only to {} over verified TLS\n", api::HOST)
}

// ── starting, input, output (the pollers' side) ──

/// `claude [args]` typed in a shell. Returns what to print now; afterwards `active(id)`
/// says whether the shell should hand its input to the session.
/// Args: `login`, `logout`, `status`, `history`, or a session with `--effort <level>`,
/// `--resume <n>`, `--cwd <dir>`, `--colors true|256|16|none`, `--base-url http://host:port`.
pub fn command(id: u64, user: &str, cwd: &str, arg: &str, colors: Colors) -> String {
    let words: Vec<&str> = arg.split_whitespace().collect();
    match words.first().copied().unwrap_or("") {
        "status" => return status_text(user),
        "logout" => {
            return if crate::creds::kernel_forget(CRED) { String::from("claude: API key erased\n") } else { String::from("claude: no API key was set (or the store is locked)\n") };
        },
        "history" => return history_text(user),
        "help" | "--help" | "-h" => return help_text(),
        // `claude tls-check [addr:port [name]]`: a verified handshake and one request without
        // any key (a 401 proves the path); with an address, the certificate is checked for
        // `name` (default api.anthropic.com) -- a stand-in server must be refused
        "tls-check" => return tls_check(words.get(1).copied(), words.get(2).copied().unwrap_or(api::HOST)),
        _ => {},
    }
    if sessions().iter().any(|s| s.id == id) {
        end(id);
    }
    let mut s = Box::new(Sess {
        id,
        user: String::from(user),
        cwd: String::from(cwd),
        colors,
        out: Vec::new(),
        at_bol: true,
        inq: VecDeque::new(),
        answer: None,
        state: St::Idle,
        cancel: false,
        task: 0,
        messages: Vec::new(),
        conv: history::new_id(),
        title: String::new(),
        started_cwd: String::from(cwd),
        effort: String::from("high"),
        target: api::Target::Anthropic,
        allow_cmds: Vec::new(),
        edits_under: None,
        tokens: 0,
        cost: 0.0,
        line: Vec::new(),
    });
    if words.first() == Some(&"login") {
        s.state = St::Key;
        s.out.push((Role::Text, String::from("Paste your Claude API key (from console.anthropic.com > API Keys); it is not shown:\n")));
        s.out.push((Role::Ask, String::from("key: ")));
        sessions().push(s);
        return String::new();
    }
    let mut i = 0;
    let mut resume: Option<usize> = None;
    while i < words.len() {
        match words[i] {
            "--effort" if i + 1 < words.len() => {
                s.effort = String::from(words[i + 1]);
                i += 1;
            },
            "--resume" if i + 1 < words.len() => {
                resume = words[i + 1].parse().ok();
                i += 1;
            },
            "--cwd" if i + 1 < words.len() => {
                s.cwd = tools::resolve(cwd, user, words[i + 1]);
                s.started_cwd = s.cwd.clone();
                i += 1;
            },
            "--colors" if i + 1 < words.len() => {
                s.colors = match words[i + 1] {
                    "none" => Colors::None,
                    "16" => Colors::C16,
                    "256" => Colors::C256,
                    _ => Colors::True,
                };
                i += 1;
            },
            "--base-url" if i + 1 < words.len() => {
                let tls = words[i + 1].starts_with("https://");
                let u = words[i + 1].trim_start_matches("http://").trim_start_matches("https://");
                let (h, p) = u.split_once(':').unwrap_or((u, if tls { "443" } else { "80" }));
                s.target = api::Target::Mock(String::from(h.trim_end_matches('/')), p.trim_end_matches('/').parse().unwrap_or(if tls { 443 } else { 80 }), tls);
                i += 1;
            },
            _ => {},
        }
        i += 1;
    }
    if !["low", "medium", "high", "xhigh", "max"].contains(&s.effort.as_str()) {
        s.effort = String::from("high");
    }
    let mock = matches!(s.target, api::Target::Mock(..));
    if !mock {
        if let Err(e) = key_for(user) {
            return format!("claude: {e}\n");
        }
    }
    for seg in style::banner(&shown_cwd(&s.cwd), &format!("{MODEL_NAME}, effort {}{}", s.effort, if mock { ", TEST SERVER (dummy key)" } else { "" })) {
        s.out.push(seg);
    }
    let sid = s.id;
    sessions().push(s);
    if let Some(n) = resume {
        resume_conv(sid, n);
    }
    let task = crate::coro::spawn_owned("claude", false, "system", Box::new(move || session_task(sid)));
    if task == 0 {
        sessions().retain(|s| s.id != sid);
        return String::from("claude: no memory for the session's task\n");
    }
    with(sid, |s| {
        s.task = task;
        s.out.push((Role::Prompt, String::from("> ")));
        s.at_bol = false;
    });
    String::new()
}

/// Is a session (or a key prompt) taking this shell's input?
pub fn active(id: u64) -> bool {
    with(id, |s| s.state != St::Ended).unwrap_or(false)
}

/// Is the next line a secret (the API key prompt)?
pub fn masked(id: u64) -> bool {
    with(id, |s| s.state == St::Key).unwrap_or(false)
}

/// 0 no session, 1 waiting for a prompt, 2 working, 3 waiting for an approval (for the tray).
pub fn state_code(id: u64) -> u8 {
    with(id, |s| match s.state {
        St::Busy => 2,
        St::Ask => 3,
        St::Ended => 0,
        _ => 1,
    })
    .unwrap_or(0)
}

/// The busiest state over every session (the desktop tray's dot).
pub fn any_state() -> u8 {
    sessions().iter().map(|s| state_code(s.id)).max().unwrap_or(0)
}

/// A finished line from the shell.
pub fn input_line(id: u64, line: &str) {
    let line = String::from(line.trim_end_matches(['\r', '\n']));
    let key_mode = with(id, |s| s.state == St::Key).unwrap_or(false);
    if key_mode {
        let k = line.trim();
        let user = with(id, |s| s.user.clone()).unwrap_or_default();
        let msg = if !plausible_key(k) {
            (Role::Error, String::from("that does not look like a Claude API key (they start with sk-ant-); nothing saved\n"))
        } else if crate::keyring::user().as_deref() != Some(user.as_str()) || !crate::keyring::is_unlocked() {
            (Role::Error, format!("the key store is {user}'s sealed store: sign in on the LingOS desktop as {user}, then run claude login again\n"))
        } else if crate::creds::kernel_set(CRED, k) {
            (Role::Ok, String::from("API key saved (sealed with your account's keyring). Run `claude` to start.\n"))
        } else {
            (Role::Error, String::from("could not save the key\n"))
        };
        with(id, |s| {
            s.out.push((Role::Text, String::from("\n")));
            s.out.push(msg);
            s.state = St::Ended;
        });
        return;
    }
    with(id, |s| match s.state {
        St::Ask => s.answer = Some(line.clone()),
        St::Ended => {},
        _ => s.inq.push_back(line.clone()),
    });
}

/// Ctrl+C: stop the reply that is being written; with none, leave the session.
pub fn interrupt(id: u64) {
    let busy = with(id, |s| matches!(s.state, St::Busy | St::Ask)).unwrap_or(false);
    if busy {
        with(id, |s| {
            s.cancel = true;
            if s.state == St::Ask {
                s.answer = Some(String::from("n stopped by the user"));
            }
        });
    } else {
        end(id);
    }
}

/// Leave the session (its task ends at its next look).
pub fn end(id: u64) {
    // the conversation is saved when it ends (turns only save every 90 s: see save_soon)
    if with(id, |s| s.state != St::Ended).unwrap_or(false) {
        save(id);
    }
    with(id, |s| {
        if s.state != St::Ended {
            s.cancel = true;
            s.state = St::Ended;
            s.out.push((Role::Dim, String::from("\n(left claude)\n")));
        }
    });
}

/// What the session printed since the last look, as ANSI text (SSH). Finished
/// sessions are dropped once their last output is taken.
pub fn take_ansi(id: u64) -> String {
    let (segs, colors) = with(id, |s| (core::mem::take(&mut s.out), s.colors)).unwrap_or((Vec::new(), Colors::None));
    let out = style::render(colors, &segs);
    reap(id);
    out
}

/// The same as styled pieces (the Terminal maps roles to its colours).
pub fn take_segs(id: u64) -> Vec<(Role, String)> {
    let segs = with(id, |s| core::mem::take(&mut s.out)).unwrap_or_default();
    reap(id);
    segs
}

fn reap(id: u64) {
    let gone = with(id, |s| s.state == St::Ended && s.out.is_empty() && (s.task == 0 || !crate::coro::alive(s.task))).unwrap_or(false);
    if gone {
        sessions().retain(|s| s.id != id);
    }
}

/// SSH line editing for a session: one typed byte -> what to echo. Enter hands the line
/// over; Ctrl+C interrupts; Ctrl+D on an empty line leaves; a key prompt echoes '*'.
pub fn ssh_byte(id: u64, b: u8) -> String {
    let masked = masked(id);
    match b {
        b'\r' | b'\n' => {
            let line = with(id, |s| core::mem::take(&mut s.line)).unwrap_or_default();
            input_line(id, &String::from_utf8_lossy(&line));
            // "\n": the SSH side turns it into "\r\n"
            String::from("\n")
        },
        0x7F | 0x08 => {
            let popped = with(id, |s| s.line.pop().is_some()).unwrap_or(false);
            if popped { String::from("\x08 \x08") } else { String::new() }
        },
        0x03 => {
            with(id, |s| s.line.clear());
            interrupt(id);
            String::from("^C\n")
        },
        0x04 => {
            let empty = with(id, |s| s.line.is_empty()).unwrap_or(true);
            if empty {
                end(id);
            }
            String::new()
        },
        0x20..=0x7E | 0x80..=0xFF => {
            // UTF-8 arrives a byte at a time: the line is kept as bytes
            with(id, |s| s.line.push(b));
            if masked {
                if b < 0x80 { String::from("*") } else { String::new() }
            } else {
                String::from_utf8_lossy(&[b]).into_owned()
            }
        },
        _ => String::new(),
    }
}

fn tls_check(addr: Option<&str>, name: &str) -> String {
    let (a, port) = match addr {
        Some(x) => {
            let (h, p) = x.split_once(':').unwrap_or((x, "443"));
            (String::from(h), p.parse().unwrap_or(443))
        },
        None => (String::from(api::HOST), 443u16),
    };
    let _g = crate::drivers::netstack::NetShare::new();
    let t0 = crate::arch::timer::now_ms();
    let mut st = match crate::lingsock::Stream::connect_verified_at(&a, port, name) {
        Ok(s) => s,
        Err(e) => return format!("tls-check: {a}:{port} as {name}: REFUSED -- {e}\n"),
    };
    let ms = crate::arch::timer::now_ms() - t0;
    let req = format!("GET /v1/models HTTP/1.1\r\nHost: {name}\r\nanthropic-version: {}\r\nConnection: close\r\n\r\n", api::VERSION);
    st.send(req.as_bytes());
    for _ in 0..40 {
        if st.plain.windows(4).any(|w| w == b"\r\n\r\n") || st.closed {
            break;
        }
        st.pump(250_000);
    }
    let status = String::from_utf8_lossy(&st.plain).lines().next().unwrap_or("(no answer)").to_string();
    st.close();
    format!("tls-check: {a}:{port} as {name}: certificate verified ({ms} ms handshake); unauthenticated request -> {status}\n")
}

// ── SSH: an interactive program of the SSH shell (sshline::register) ──

/// Make `claude` an interactive command of the SSH shell.
pub fn register() {
    crate::drivers::sshline::register("claude", ssh_start);
}

static mut NEXT_SSH: u64 = 0;

fn ssh_start(arg: &str, user: &str, cwd: &str, tty: crate::drivers::sshline::TtyRef) -> bool {
    let id = unsafe {
        NEXT_SSH += 1;
        NEXT_SSH
    };
    let first = command(id, user, cwd, arg, Colors::True);
    tty.borrow_mut().output.extend_from_slice(first.as_bytes());
    if !active(id) {
        // a one-shot subcommand (status, logout, history, help) or an error
        tty.borrow_mut().done = true;
        return true;
    }
    crate::coro::spawn_owned("claude-ssh", false, "system", Box::new(move || ssh_bridge(id, tty))) != 0
}

/// Typed bytes -> the session; what it printed -> the client; done when it ends. The SSH
/// task drops its side of the tty when the client goes away: the session ends with it.
fn ssh_bridge(id: u64, tty: crate::drivers::sshline::TtyRef) {
    loop {
        if alloc::rc::Rc::strong_count(&tty) == 1 {
            end(id);
            let _ = take_ansi(id);
            return;
        }
        let input = core::mem::take(&mut tty.borrow_mut().input);
        let mut echo = String::new();
        for b in input {
            echo.push_str(&ssh_byte(id, b));
        }
        let out = take_ansi(id);
        {
            let mut t = tty.borrow_mut();
            t.output.extend_from_slice(echo.as_bytes());
            t.output.extend_from_slice(out.as_bytes());
        }
        if !active(id) {
            let rest = take_ansi(id);
            let mut t = tty.borrow_mut();
            t.output.extend_from_slice(rest.as_bytes());
            t.done = true;
            return;
        }
        crate::coro::sleep_ms(20);
    }
}

// ── the session task ──

fn session_task(id: u64) {
    loop {
        let line = loop {
            match with(id, |s| (s.state, s.inq.pop_front())) {
                None | Some((St::Ended, _)) => {
                    with(id, |s| s.task = 0);
                    return;
                },
                Some((_, Some(l))) => break l,
                _ => crate::coro::sleep_ms(40),
            }
        };
        let t = line.trim();
        if t.is_empty() {
            prompt(id);
            continue;
        }
        if t.starts_with('/') {
            slash(id, t);
            if !active(id) {
                with(id, |s| s.task = 0);
                return;
            }
            prompt(id);
            continue;
        }
        turn(id, t);
        save_soon(id);
        prompt(id);
    }
}

fn prompt(id: u64) {
    nl(id);
    let st = with(id, |s| s.state).unwrap_or(St::Ended);
    if st != St::Ended {
        say(id, Role::Prompt, "> ");
    }
}

fn help_text() -> String {
    String::from(
        "claude            start a session in this folder (Ctrl+C stops a reply; /exit leaves)\n\
         claude login      save an API key (console.anthropic.com > API Keys)\n\
         claude logout     erase it\n\
         claude status     what is set\n\
         claude history    saved conversations\n\
         claude --resume <n> | --effort low|medium|high|xhigh|max | --cwd <dir> | --colors true|256|16|none\n\
         in a session: /new  /history  /resume <n>  /delete <n>  /effort <level>  /model  /cost  /cwd  /exit\n",
    )
}

fn history_text(user: &str) -> String {
    let l = history::list(user);
    if l.is_empty() {
        return String::from("no saved conversations (they are sealed: sign in on the desktop to read them)\n");
    }
    let mut out = String::new();
    for (i, (_, t, c, n, _)) in l.iter().enumerate() {
        out.push_str(&format!("{:>3}  {}  ({} messages, /{})\n", i + 1, t, n, c));
    }
    out
}

fn slash(id: u64, t: &str) {
    let (cmd, arg) = t.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((t, ""));
    let user = with(id, |s| s.user.clone()).unwrap_or_default();
    match cmd {
        "/exit" | "/quit" => end(id),
        "/new" | "/clear" => {
            save(id);
            with(id, |s| {
                s.messages.clear();
                s.conv = history::new_id();
                s.title.clear();
                s.started_cwd = s.cwd.clone();
                s.allow_cmds.clear();
                s.edits_under = None;
            });
            say(id, Role::Ok, "new conversation\n");
        },
        "/history" => say(id, Role::Text, &history_text(&user)),
        "/resume" => match arg.parse::<usize>() {
            Ok(n) => {
                save(id);
                resume_conv(id, n);
            },
            Err(_) => say(id, Role::Error, "usage: /resume <n> (numbers from /history)\n"),
        },
        "/delete" => match arg.parse::<usize>().ok().and_then(|n| history::list(&user).get(n.wrapping_sub(1)).cloned()) {
            Some((cid, title, ..)) => {
                if history::delete(&user, &cid) {
                    say(id, Role::Ok, &format!("deleted \"{title}\"\n"));
                }
            },
            None => say(id, Role::Error, "usage: /delete <n> (numbers from /history)\n"),
        },
        "/effort" => {
            if ["low", "medium", "high", "xhigh", "max"].contains(&arg) {
                with(id, |s| s.effort = String::from(arg));
                say(id, Role::Ok, &format!("effort {arg}\n"));
            } else {
                let e = with(id, |s| s.effort.clone()).unwrap_or_default();
                say(id, Role::Text, &format!("effort {e} (choose low, medium, high, xhigh or max)\n"));
            }
        },
        "/model" => say(id, Role::Text, &format!("{MODEL_NAME} ({MODEL}); refused replies fall back to another model server-side\n")),
        "/cost" => {
            let (t, c) = with(id, |s| (s.tokens, s.cost)).unwrap_or((0, 0.0));
            say(id, Role::Dim, &format!("this session: {t} tokens, about ${c:.4}\n"));
        },
        "/cwd" | "/pwd" => {
            let c = with(id, |s| s.cwd.clone()).unwrap_or_default();
            say(id, Role::Where, &format!("{}\n", shown_cwd(&c)));
        },
        "/cd" => {
            let c = with(id, |s| s.cwd.clone()).unwrap_or_default();
            let p = tools::resolve(&c, &user, arg);
            if p.is_empty() || crate::vfs::is_dir(&p) {
                with(id, |s| s.cwd = p.clone());
                say(id, Role::Where, &format!("{}\n", shown_cwd(&p)));
            } else {
                say(id, Role::Error, &format!("/{p}: no such folder\n"));
            }
        },
        "/help" => say(id, Role::Text, &help_text()),
        _ => say(id, Role::Error, &format!("{cmd}: unknown (/help)\n")),
    }
}

fn resume_conv(id: u64, n: usize) {
    let user = with(id, |s| s.user.clone()).unwrap_or_default();
    let Some((cid, title, cwd, count, _)) = history::list(&user).get(n.wrapping_sub(1)).cloned() else {
        say(id, Role::Error, "no such conversation (/history lists them)\n");
        return;
    };
    let Some(doc) = history::load(&user, &cid) else {
        say(id, Role::Error, "could not open it (is the desktop signed in as you?)\n");
        return;
    };
    let msgs = doc.get("messages").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    with(id, |s| {
        s.messages = msgs;
        s.conv = cid;
        s.title = title.clone();
        s.cwd = cwd.clone();
        s.started_cwd = cwd.clone();
    });
    say(id, Role::Ok, &format!("resumed \"{title}\" ({count} messages) in "));
    say(id, Role::Where, &format!("{}\n", shown_cwd(&cwd)));
}

/// After a turn: save if the last save is 90 s old or more. Sealing + writing the whole history
/// stalled the desktop 1.2-4.7 s per save in QEMU when it ran after every turn; `end`, `/new`
/// and `/resume` still save every time, so at most ~90 s of a crashed session can be lost.
fn save_soon(id: u64) {
    static mut LAST: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
    let now = crate::arch::timer::now_ms();
    let last = unsafe { &mut *(&raw mut LAST) };
    let due = match last.iter_mut().find(|(i, _)| *i == id) {
        Some(e) if now.saturating_sub(e.1) < 90_000 => false,
        Some(e) => {
            e.1 = now;
            true
        },
        None => {
            last.push((id, now));
            true
        },
    };
    if due {
        save(id);
    }
}

fn save(id: u64) {
    let Some((user, conv, title, cwd, msgs, cost, tok)) = with(id, |s| (s.user.clone(), s.conv.clone(), s.title.clone(), s.started_cwd.clone(), s.messages.clone(), s.cost, s.tokens)) else { return };
    if msgs.is_empty() {
        return;
    }
    if !history::save(&user, &conv, &history::doc(&title, &cwd, MODEL, &msgs, cost, tok)) {
        say(id, Role::Dim, "(not saved: the history is sealed with your keyring, which is locked or another user's)\n");
    }
}

fn body(id: u64) -> Value {
    let (msgs, effort) = with(id, |s| (s.messages.clone(), s.effort.clone())).unwrap_or_default();
    json!({
        "model": MODEL,
        "max_tokens": 64000,
        "stream": true,
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": effort},
        "system": [{"type": "text", "text": SYSTEM, "cache_control": {"type": "ephemeral"}}],
        "tools": tools::definitions(),
        "messages": msgs,
        "fallbacks": "default",
        "cache_control": {"type": "ephemeral"}
    })
}

/// One user prompt, through as many tool rounds as Claude needs.
fn turn(id: u64, text: &str) {
    let Some((user, cwd, first)) = with(id, |s| (s.user.clone(), s.cwd.clone(), s.messages.is_empty())) else { return };
    let key = match with(id, |s| s.target.clone()) {
        Some(api::Target::Anthropic) => match key_for(&user) {
            Ok(k) => k,
            Err(e) => {
                say(id, Role::Error, &format!("{e}\n"));
                return;
            },
        },
        _ => String::new(),
    };
    // the working directory goes in the prompt (not the system prompt, which stays byte-stable for caching)
    let mut content = Vec::new();
    content.push(json!({"type": "text", "text": format!("[LingOS session: user {user}, working directory {}]", shown_cwd(&cwd))}));
    content.push(json!({"type": "text", "text": text}));
    let start = with(id, |s| {
        let n = s.messages.len();
        // after a reply stopped during approvals the history ends with a user turn (the tool
        // results): add this prompt to it rather than sending two user turns in a row
        let last_user = s.messages.last().and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("user");
        if last_user {
            if let Some(arr) = s.messages.last_mut().and_then(|m| m.get_mut("content")).and_then(|c| c.as_array_mut()) {
                arr.extend(content.iter().cloned());
            }
        } else {
            s.messages.push(json!({"role": "user", "content": content}));
        }
        if first {
            s.title = text.chars().take(60).collect();
        }
        s.cancel = false;
        n
    })
    .unwrap_or(0);
    let undo = |why: &str| {
        with(id, |s| s.messages.truncate(start));
        nl(id);
        say(id, Role::Error, why);
    };
    for _round in 0..50 {
        with(id, |s| s.state = St::Busy);
        let req = body(id);
        let target = with(id, |s| s.target.clone()).unwrap_or(api::Target::Anthropic);
        let mut thinking_shown = false;
        // the request's stage and when it began: one that takes long is shown (a silent
        // cursor for minutes looked like a dead session when a Wi-Fi upload stalled)
        let stage: core::cell::RefCell<(String, u64, u64)> = core::cell::RefCell::new((String::new(), 0, 0));
        let res = api::stream(
            &target,
            &key,
            &req,
            &mut |ev| match ev {
                api::Event::Stage(what) => *stage.borrow_mut() = (String::from(what), crate::arch::timer::now_ms(), 4_000),
                api::Event::Text(t) => say(id, Role::Text, t),
                api::Event::BlockStart("thinking", _) | api::Event::BlockStart("redacted_thinking", _) => {
                    if !thinking_shown {
                        thinking_shown = true;
                        nl(id);
                        say(id, Role::Dim, "(thinking...)\n");
                    }
                },
                api::Event::BlockStart("text", _) => nl(id),
                _ => {},
            },
            &mut || {
                crate::coro::yield_now();
                let mut g = stage.borrow_mut();
                let (what, since, next) = &mut *g;
                let waited = crate::arch::timer::now_ms().saturating_sub(*since);
                if !what.is_empty() && waited >= *next {
                    *next = (*next * 2).max(waited + 1);
                    nl(id);
                    say(id, Role::Dim, &format!("({what}... {} s; Ctrl+C stops)\n", waited / 1000));
                }
                with(id, |s| s.cancel).unwrap_or(true)
            },
        );
        let reply = match res {
            Ok(r) => r,
            Err(e) if e == "cancelled" => {
                undo("(stopped; that prompt was not kept)\n");
                with(id, |s| {
                    s.state = St::Idle;
                    s.cancel = false;
                });
                return;
            },
            Err(e) => {
                undo(&format!("claude: {e}\n"));
                with(id, |s| s.state = St::Idle);
                return;
            },
        };
        let u = reply.usage;
        with(id, |s| {
            s.tokens += u.input + u.output + u.cache_read + u.cache_write;
            s.cost += u.dollars();
        });
        // tool calls whose input did not parse: answer them as errors, never run them
        let mut content = reply.content;
        let mut bad: Vec<(String, String)> = Vec::new();
        for b in content.iter_mut() {
            if let Some(raw) = b.get("_invalid_json").and_then(|x| x.as_str()).map(String::from) {
                if let Some(o) = b.as_object_mut() {
                    o.remove("_invalid_json");
                }
                bad.push((b.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(), raw));
            }
        }
        let has_tools = content.iter().any(|b| b.get("type").and_then(|x| x.as_str()) == Some("tool_use"));
        let stop = reply.stop_reason.clone();
        // a turn cut off mid-tool-call (max_tokens, refusal) is not kept: its tool calls would need answers
        if (stop == "max_tokens" || stop == "refusal") && has_tools {
            undo(if stop == "refusal" { "the reply was declined mid-way; nothing was run\n" } else { "the reply ran out of room mid-way; nothing was run (try a smaller step)\n" });
            show_refusal(id, &reply.stop_details);
            with(id, |s| s.state = St::Idle);
            return;
        }
        with(id, |s| s.messages.push(json!({"role": "assistant", "content": content.clone()})));
        footer(id, &u);
        match stop.as_str() {
            "tool_use" => {
                let results = run_tools(id, &content, &bad);
                if with(id, |s| s.cancel).unwrap_or(true) {
                    // stopped during approvals: keep the answers given so far, end the turn
                    with(id, |s| s.messages.push(json!({"role": "user", "content": results})));
                    say(id, Role::Dim, "(stopped)\n");
                    with(id, |s| {
                        s.state = St::Idle;
                        s.cancel = false;
                    });
                    return;
                }
                with(id, |s| s.messages.push(json!({"role": "user", "content": results})));
            },
            "pause_turn" => {},
            "refusal" => {
                nl(id);
                say(id, Role::Error, "Claude declined this request.\n");
                show_refusal(id, &reply.stop_details);
                break;
            },
            "max_tokens" => {
                nl(id);
                say(id, Role::Dim, "(the reply hit its length limit)\n");
                break;
            },
            _ => break,
        }
    }
    with(id, |s| s.state = St::Idle);
}

fn show_refusal(id: u64, d: &Value) {
    let cat = d.get("category").and_then(|x| x.as_str()).unwrap_or("");
    let why = d.get("explanation").and_then(|x| x.as_str()).unwrap_or("");
    if !cat.is_empty() || !why.is_empty() {
        say(id, Role::Dim, &format!("({}{}{})\n", cat, if cat.is_empty() || why.is_empty() { "" } else { ": " }, why));
    }
}

fn footer(id: u64, u: &api::Usage) {
    let (t, c) = with(id, |s| (s.tokens, s.cost)).unwrap_or((0, 0.0));
    nl(id);
    say(
        id,
        Role::Dim,
        &format!("  [{} in + {} cached + {} out tokens, ${:.4}; session {} tokens, ${:.4}]\n", u.input + u.cache_write, u.cache_read, u.output, u.dollars(), t, c),
    );
}

/// Run one assistant turn's tool calls (asking where needed); every tool_use gets a result.
fn run_tools(id: u64, content: &[Value], bad: &[(String, String)]) -> Vec<Value> {
    let mut results = Vec::new();
    for b in content {
        if b.get("type").and_then(|x| x.as_str()) != Some("tool_use") {
            continue;
        }
        let tid = b.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = b.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let inp = b.get("input").cloned().unwrap_or(json!({}));
        let err = |t: String| json!({"type": "tool_result", "tool_use_id": tid, "is_error": true, "content": t});
        if let Some((_, raw)) = bad.iter().find(|(i, _)| *i == tid) {
            results.push(err(json!({"INVALID_JSON": raw}).to_string()));
            continue;
        }
        if with(id, |s| s.cancel).unwrap_or(true) {
            results.push(err(String::from("not run: the user stopped the reply")));
            continue;
        }
        if let Err(e) = tools::validate(&name, &inp) {
            results.push(err(format!("invalid input: {e}")));
            continue;
        }
        let (user, cwd) = with(id, |s| (s.user.clone(), s.cwd.clone())).unwrap_or_default();
        nl(id);
        let ok = match tools::need(&name, &inp, &cwd, &user) {
            tools::Need::None => {
                let p = inp.get("path").and_then(|x| x.as_str()).unwrap_or("");
                let what = match name.as_str() {
                    "search" => format!("search {:?} in {p}\n", inp.get("pattern").and_then(|x| x.as_str()).unwrap_or("")),
                    _ => format!("{name} {p}\n"),
                };
                say(id, Role::Dim, "  ");
                say(id, Role::Path, &what);
                Ok(())
            },
            tools::Need::Shell(cmd, ro) => {
                say(id, Role::Dim, "  $ ");
                say(id, Role::Cmd, &format!("{cmd}\n"));
                let pre = ro.as_ref().map(|c| with(id, |s| s.allow_cmds.contains(c)).unwrap_or(false)).unwrap_or(false);
                let r = if pre { Ok('y') } else { ask(id, ro.is_some(), false) };
                r.map(|a| {
                    if a == 'a' {
                        if let Some(c) = ro {
                            with(id, |s| s.allow_cmds.push(c));
                        }
                    }
                })
            },
            tools::Need::Edit(what, path) => {
                say(id, Role::Dim, "  ");
                say(id, Role::Path, &format!("{what}\n"));
                let (del, add) = tools::edit_preview(&inp);
                for l in del {
                    say(id, Role::Del, &format!("  - {l}\n"));
                }
                for l in add {
                    say(id, Role::Add, &format!("  + {l}\n"));
                }
                let under = with(id, |s| s.edits_under.clone()).flatten();
                let pre = under.as_ref().map(|u| u.is_empty() || path == *u || path.starts_with(&format!("{u}/"))).unwrap_or(false);
                let r = if pre { Ok('y') } else { ask(id, false, true) };
                r.map(|a| {
                    if a == 'e' {
                        let root = with(id, |s| s.started_cwd.clone()).unwrap_or_default();
                        with(id, |s| s.edits_under = Some(root.clone()));
                        say(id, Role::Ok, &format!("  (file edits under {} are accepted for this session)\n", shown_cwd(&root)));
                    }
                })
            },
        };
        match ok {
            Err(reason) => results.push(err(format!("the user declined{}{}", if reason.is_empty() { "" } else { ": " }, reason))),
            Ok(()) => {
                let mut cwd2 = cwd.clone();
                let r = tools::run(&name, &inp, &mut cwd2, &user);
                if cwd2 != cwd {
                    with(id, |s| s.cwd = cwd2.clone());
                    say(id, Role::Where, &format!("  now in {}\n", shown_cwd(&cwd2)));
                }
                match r {
                    Ok(t) => {
                        if name == "lingos_shell" {
                            let shown: String = t.lines().take(8).map(|l| format!("    {l}\n")).collect();
                            say(id, Role::Dim, &shown);
                            if t.lines().count() > 8 {
                                say(id, Role::Dim, &format!("    ... ({} lines)\n", t.lines().count()));
                            }
                        } else if name == "edit_file" || name == "write_file" {
                            say(id, Role::Ok, &format!("  {}\n", t.lines().next().unwrap_or("")));
                        }
                        results.push(json!({"type": "tool_result", "tool_use_id": tid, "content": t}));
                    },
                    Err(e) => {
                        say(id, Role::Error, &format!("  {e}\n"));
                        results.push(err(e));
                    },
                }
            },
        }
    }
    results
}

/// Ask for approval: Ok(answer letter) or Err(reason for "no").
fn ask(id: u64, offer_always: bool, offer_edits: bool) -> Result<char, String> {
    let mut q = String::from("  Allow? [y]es / [n]o <reason>");
    if offer_edits {
        q.push_str(" / [e] all file edits in this project");
    }
    if offer_always {
        q.push_str(" / [a] always this command");
    }
    q.push_str(": ");
    with(id, |s| {
        s.answer = None;
        s.state = St::Ask;
    });
    say(id, Role::Ask, &q);
    loop {
        crate::coro::sleep_ms(40);
        let Some((a, cancel, st)) = with(id, |s| (s.answer.take(), s.cancel, s.state)) else { return Err(String::from("session ended")) };
        if st == St::Ended || (cancel && a.is_none()) {
            return Err(String::from("stopped by the user"));
        }
        let Some(a) = a else { continue };
        let t = a.trim();
        let first = t.chars().next().unwrap_or(' ').to_ascii_lowercase();
        let rest = t.get(1..).unwrap_or("").trim().to_string();
        let r = match first {
            'y' => Ok('y'),
            'e' if offer_edits => Ok('e'),
            'a' if offer_always => Ok('a'),
            'n' => Err(rest),
            _ => {
                say(id, Role::Ask, &q);
                continue;
            },
        };
        with(id, |s| s.state = St::Busy);
        return r;
    }
}
