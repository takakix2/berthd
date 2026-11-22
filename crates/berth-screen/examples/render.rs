//! stdin の録画を `cols × rows` で描き直して stdout へ（`scripts/screen-oracle.sh` の片腕）。
//!
//!     render <cols> <rows> [--text] < fixture.bin
//!
//! `--text` は制御列を落とした文字だけ（xterm.js の物差しと突き合わせる形）。

use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let text = args.iter().any(|a| a == "--text");
    let nums: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (cols, rows) = match nums.as_slice() {
        [c, r] => (c.parse::<u16>().ok(), r.parse::<u16>().ok()),
        _ => (None, None),
    };
    let (Some(cols), Some(rows)) = (cols, rows) else {
        eprintln!("usage: render <cols> <rows> [--text] < replay");
        std::process::exit(2);
    };
    let mut replay = Vec::new();
    std::io::stdin()
        .read_to_end(&mut replay)
        .expect("read stdin");
    let r = berth_screen::render(&replay, cols, rows);
    let mut out = std::io::stdout().lock();
    if text {
        let mut s = berth_screen::plain_text(&r.scrollback);
        s.push_str(&berth_screen::plain_text(&r.screen));
        // 物差しは行ごとに比べるので `\n` に揃える。
        out.write_all(s.replace("\r\n", "\n").as_bytes()).unwrap();
        out.write_all(b"\n").unwrap();
    } else {
        out.write_all(&r.scrollback).unwrap();
        out.write_all(&r.screen).unwrap();
    }
}
