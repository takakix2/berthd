# fixtures: `scripts/screen-oracle.sh` が xterm.js と突き合わせる録画

名前は `<何>.<cols>x<rows>.bin`。中身は端末の録画そのもの（`\r\n` と ESC を含む生バイト・
`.gitattributes` で binary）。単体検体（`src/lib.rs`）と同じ物をファイルにも置いてある。

## `.known-diff.`（違うことが分かっている物）

| fixture | 何が違うか | なぜそのまま |
|---|---|---|
| `clear.known-diff.80x24` | `ESC[2J`（画面消去）の後、**alacritty は消した画面を履歴へ押し出す**（`Grid::clear_viewport` が scroll_up してから消す）。xterm.js は捨てる。∴ こちらの出力には `old` が履歴として残り、xterm.js には無い | 実端末も割れている（xterm は捨てる・VTE / Windows Terminal は履歴に残す）。消した中身が**スクロールに残る**方へ倒すのは「画面が壊れて見える」側ではないので、alacritty の形をそのまま採る。重要: 一致するようになったら版が動いた印なので読み直す |
