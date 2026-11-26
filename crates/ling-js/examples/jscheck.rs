// parse + compile files without running them: cargo run --example jscheck -- a.js b.js
use ling_js::Engine;

fn main() {
    let mut e = Engine::new();
    for path in std::env::args().skip(1) {
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        let t = std::time::Instant::now();
        let r = match ling_js::parser::parse_program(&src) {
            Err(err) => {
                let line = src.lines().nth(err.line.saturating_sub(1) as usize).unwrap_or("");
                format!("PARSE ERROR {} (line {}): {}", err.msg, err.line, line.chars().take(160).collect::<String>())
            },
            Ok(p) => {
                let c = ling_js::compiler::Compiler::new(&mut e.atoms, &src);
                match c.compile_script(&p) {
                    Ok(_) => "ok".into(),
                    Err(err) => format!("COMPILE ERROR {}", err.0),
                }
            },
        };
        let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        println!("{:>9} bytes {:>5} ms  {}  {}", src.len(), t.elapsed().as_millis(), name, r);
    }
}
