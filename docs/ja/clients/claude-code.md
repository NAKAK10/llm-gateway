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

## Claude Code を Anthropic 以外のプロバイダーへルーティングする

Claude Code は Anthropic Messages しか話しませんが、ゲートウェイは
`anthropic-messages` → `openai-chat` を変換するので、OpenAI 互換の
プロバイダーなら送り先として何でも使えます — Ollama(ローカル/クラウド)、
Groq、DeepSeek、Gemini、Mistral、Together、Sakana AI、PLaMo。

普通の名前でルートを追加します(ルート名に `:` や `/` は使えないので、
プロバイダー自身のモデル id は右辺にそのまま残せます):

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
},
routes: {
  "role-cheap": {
    description: "短い定型作業: 要約、整形、コミットメッセージ生成",
    model: { default: "ollama-local/qwen3:8b" },
  },
}
```

あとはこれを選ぶだけです。セッションごとに指定するなら:

```sh
llm-gateway launch claude --model role-cheap
```

あるいは Claude Code の中から `/model role-cheap` — 名前はルート名と
完全一致している必要があります。

このようなルートで諦めることになるものは README にまとまっています
([クロスプロトコルルーティング](../../../README.ja.md#クロスプロトコルルーティング))。
手短に言えば、プロンプトキャッシュ、thinking ブロック、Anthropic の
サーバーサイドツール、そして正確なトークン数です。`llm-gateway trace` は
これらのリクエストに `xlat=anthropic-messages->openai-chat` を付けます。

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
| `openai-chat` のルートでコンテキストサイズがおかしく見える | 想定どおり: そのプロトコルにはカウント用エンドポイントがないため、数値はローカルの推定値 |
| ローカルモデルが最初は何も答えず、途中から急に答え始めるように見える | `reasoning_content` が変換で破棄されているだけ; 転送されるのは答えの部分のみ |

## 元に戻す

`ANTHROPIC_BASE_URL` の行を削除して Claude Code を再起動。1 行だけです。
