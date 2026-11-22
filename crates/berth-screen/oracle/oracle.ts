// xterm.js を物差しにする。
//
// stdin の録画（生バイト）を cols×rows の headless xterm.js に流し、文字だけを行ごとに stdout へ出す。
// `cargo run -p berth-screen --example render -- <cols> <rows> --text` と同じ形で比べる（scripts/screen-oracle.sh）。
//
//   bun run oracle.ts <cols> <rows> [--attrs] < fixture.bin
//
// `--attrs` は文字に加えてセルの色・属性も出す（属性が変わるところに `«…»` の印）。
// berth-screen の出力をもう一度この物差しに通して元の録画と比べる ＝ 端末が受け取るのと同じ経路で色を確かめる。
//
// ⚠️ 開発のときだけ使う（bun と node_modules が要る）。配る `berth` には入れない。
// 版は package.json で固定（xterm.js 5.5 系）。

import { Terminal } from "@xterm/headless";

const args = process.argv.slice(2);
const attrs = args.includes("--attrs");
const [colsArg, rowsArg] = args.filter((a) => !a.startsWith("--"));
const cols = Number(colsArg);
const rows = Number(rowsArg);
if (!Number.isInteger(cols) || !Number.isInteger(rows) || cols < 2 || rows < 1) {
  console.error("usage: bun run oracle.ts <cols> <rows> [--attrs] < replay");
  process.exit(2);
}

const chunks: Uint8Array[] = [];
for await (const chunk of Bun.stdin.stream()) chunks.push(chunk);
const replay = Buffer.concat(chunks.map((c) => Buffer.from(c)));

const term = new Terminal({ cols, rows, scrollback: 100_000, allowProposedApi: true });
await new Promise<void>((resolve) => term.write(replay, resolve));

// 履歴 + 画面の文字を、berth-screen と同じ規則で並べる:
//   - 末尾の空白は切る（translateToString(true)）
//   - 端末が折った行（isWrapped）は前の行に繋げる（折れ目に改行を入れない）
//   - alt screen の中なら履歴は出さず、画面だけ
const buf = term.buffer.active;

// セルの色と属性を 1 つの鍵に。色は「どの面の色か」（既定 / 16・256 色 / RGB）と値の組。
type Cell = NonNullable<ReturnType<NonNullable<ReturnType<typeof buf.getLine>>["getCell"]>>;
function attrKey(c: Cell): string {
  const colour = (mode: string, v: number) => (mode === "d" ? "d" : `${mode}${v}`);
  const fg = c.isFgDefault() ? "d" : colour(c.isFgRGB() ? "rgb" : "p", c.getFgColor());
  const bg = c.isBgDefault() ? "d" : colour(c.isBgRGB() ? "rgb" : "p", c.getBgColor());
  const flags = [
    c.isBold() ? "b" : "",
    c.isDim() ? "f" : "",
    c.isItalic() ? "i" : "",
    c.isUnderline() ? "u" : "",
    c.isBlink() ? "k" : "",
    c.isInverse() ? "r" : "",
    c.isInvisible() ? "h" : "",
    c.isStrikethrough() ? "s" : "",
  ].join("");
  return `${fg}/${bg}${flags ? "/" + flags : ""}`;
}

// 1 行を `«鍵»文字…` に。⚠️ 行末の「既定の属性の空白」は切る（文字だけの比べ方と同じ理由）——
// 色の付いた空白（背景色）は見えるので残す。
// 折れた行は前の行に繋ぐので、印も前の行の最後の属性から続ける（`carry`）。
let carry = "d/d";
function attrText(line: NonNullable<ReturnType<typeof buf.getLine>>, from: string): string {
  const cells: { ch: string; key: string }[] = [];
  for (let x = 0; x < line.length; x++) {
    const c = line.getCell(x);
    if (!c) continue;
    if (c.getWidth() === 0) continue; // 幅 2 の文字の後ろ半分
    cells.push({ ch: c.getChars() || " ", key: attrKey(c) });
  }
  while (cells.length > 0 && cells[cells.length - 1].ch === " " && cells[cells.length - 1].key === "d/d") cells.pop();
  let out = "";
  let cur = from;
  for (const { ch, key } of cells) {
    if (key !== cur) {
      out += `«${key}»`;
      cur = key;
    }
    out += ch;
  }
  carry = cur;
  return out;
}
const inAlt = term.buffer.active.type === "alternate";
const lines: string[] = [];
const first = inAlt ? buf.baseY : 0;
for (let y = first; y < buf.length; y++) {
  const line = buf.getLine(y);
  if (!line) continue;
  const joined = line.isWrapped && lines.length > 0;
  const text = attrs ? attrText(line, joined ? carry : "d/d") : line.translateToString(true);
  if (joined) lines[lines.length - 1] += text;
  else lines.push(text);
}
// berth-screen の `--text` は履歴の各行 + 画面 rows 行（最後だけ改行無し）に末尾 1 つの `\n` を足す ＝
// 行ごとに 1 本の `\n`。同じ形にする。
process.stdout.write(lines.join("\n") + "\n");
