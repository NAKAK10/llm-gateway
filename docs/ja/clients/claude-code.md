# Claude Code — 手動セットアップ

[English](../../clients/claude-code.md) | 日本語

サポートされている方法は `llm-gateway launch claude` で、その場合この
ページの作業は一切不要です。このページはリダイレクトを手動で*恒久化*する
ためのものです。ゲートウェイ自身がこのファイルを書き換えることはありません。

## Claude Code をゲートウェイに向ける

`~/.claude/settings.json` に追記:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:4000",
    "ANTHROPIC_AUTH_TOKEN": "<server.apiKey の値>",
    "ANTHROPIC_CUSTOM_HEADERS": "x-gw-client: claude-code",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
  }
}
```

既存の `"model"` 設定はそのままにしてください — ゲートウェイの `claude-*`
ワイルドカードルートは、バックグラウンドリクエスト用の小型モデルも含めて
Claude Code が解決した id をそのまま転送するため、Anthropic が新しい id を
出しても壊れません。

補足:

- `ANTHROPIC_AUTH_TOKEN` には自動で `Bearer ` が前置されます。
  `ANTHROPIC_API_KEY` を**併用しない**こと — どちらか片方だけにしないと、
  どちらのヘッダーが勝ったのかを突き止めるのに午後が丸ごと潰れます。
- settings の `env` ブロックの値は**シェル環境を上書き**します。だからこそ
  `launch claude` はこのファイルが既に `ANTHROPIC_BASE_URL` を設定している
  場合に警告します — 2 つの仕組みは衝突し、settings.json が勝ちます。
- トークンをファイルに書きたくない場合は、`"apiKeyHelper"` にトークンを
  出力するスクリプトを指定してください。

## 動作確認

```sh
llm-gateway trace --tail     # 別ターミナルで、serve --debug と併用
claude -p "ping"             # client=claude-code のトレース行が出ること
```

次に逆のテスト — ゲートウェイを停止して `claude -p "ping"` が**失敗する**
ことを確認します。成功してしまうなら Anthropic に直接話しており、
トラフィックはゲートウェイを一切通っていません。

## トラブルシューティング

| 症状 | 対処 |
|---|---|
| betas / `context_management` に言及する 400 | `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`(切り分けのため一時的に) |
| settings の変更が反映されない | settings の `env` はシェルの export を上書きする — シェルではなくファイルを確認 |
| count_tokens のエラー | ゲートウェイは `/v1/messages/count_tokens` を転送する; `llm-gateway providers` を確認 |

## 元に戻す

`ANTHROPIC_BASE_URL` の行を削除して Claude Code を再起動。1 行だけです。
