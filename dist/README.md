# dist — berthd を常駐させる

他のマシンやモバイルの端末アプリから `ssh` で入って `berth attach` する使い方では、そのマシンで
berthd が**常駐**している必要があります。ここにはそのための service 定義を置いています。

先に `berthd` と `berth` を `~/.local/bin` に入れておきます（[README](../README.md) の「入れる」）。

## Linux（systemd user service）

```sh
mkdir -p ~/.config/systemd/user
cp dist/berthd.service ~/.config/systemd/user/
loginctl enable-linger "$USER"        # ログアウト後も動かし続ける（常駐させるなら必須）
systemctl --user daemon-reload
systemctl --user enable --now berthd
berth info                             # 動作確認
```

停止は `systemctl --user stop berthd` です（service 管理下では `berth kill-server` を使いません）。

## macOS（launchd LaunchAgent）

```sh
# plist の ProgramArguments のパス（/Users/CHANGEME）を自分の $HOME に直してから:
cp dist/com.berth.berthd.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist
berth info
```

停止は `launchctl unload ~/Library/LaunchAgents/com.berth.berthd.plist` です。

## service 管理下では自動起動を切る（推奨）

berthd を service に任せるなら、その環境で **`BERTHD_NO_SPAWN=1`** を立てておくと、service が
一瞬落ちた隙に `berth attach` が**別の berthd を立てて二重になる**のを防げます。`~/.profile` などに:

```sh
export BERTHD_NO_SPAWN=1
```

service を使わず「最初の `berth attach` で立てる」運用なら、これは設定しません。

## 旧 `argod` / `argo` から乗り換える

バイナリだけ入れ替えても、unit が古い名前を起こしに行きます。unit も入れ替えてください。

```sh
# Linux (systemd --user)
systemctl --user disable --now argod          # 旧 unit を止めてから
rm -f ~/.config/systemd/user/argod.service
cp dist/berthd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now berthd

# macOS (launchd)
launchctl unload ~/Library/LaunchAgents/com.argo.argod.plist   # 旧 Label を外してから
rm -f ~/Library/LaunchAgents/com.argo.argod.plist
cp dist/com.berth.berthd.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/com.berth.berthd.plist

# 旧バイナリを消す（installer を使った場合は消えています）
rm -f ~/.local/bin/argod ~/.local/bin/argo
```

旧バイナリは必ず消してください。`argo` / `argod` は新しい berthd と**別のソケット**を見るので、
繋がらないか、古いデーモンに繋がって「セッションが無い」と答えます。
