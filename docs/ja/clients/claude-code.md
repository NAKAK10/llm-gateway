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

Claude 側の `model` 設定は残しておいて構いませんが、もう route 制御だとは
思わないでください。Claude は依然として model 文字列を必要としますが、
`llm-gateway` は route 選択の際にその文字列を無視し、代わりに全リクエストを
route の `description` に対して内容分類します。したがって
`llm-gateway launch claude` は `ANTHROPIC_MODEL` に固定リテラル `default`
を入れ、Claude 自身の `/model` ピッカーもゲートウェイの routing という意味では
飾りです。

補足:

- `ANTHROPIC_AUTH_TOKEN` には自動で `Bearer ` が前置されます。
  `ANTHROPIC_API_KEY` を**併用しない**こと — どちらか片方だけにしないと、
  どちらのヘッダーが勝ったのかを突き止めるのに午後が丸ごと潰れます。
- settings の `env` ブロックの値は**シェル環境を上書き**します。だからこそ
  `launch claude` はこのファイルが既に `ANTHROPIC_BASE_URL` を設定している
  場合に警告します — 2 つの仕組みは衝突し、settings.json が勝ちます。
- トークンをファイルに書きたくない場合は、`"apiKeyHelper"` にトークンを
  出力するスクリプトを指定してください。

## 「API キーではなく Claude のサブスクリプションを持っている」場合

モードは 3 つあり、起動ごとに選ぶことになります。

| 実行するもの | 認証するのは | 得られるもの |
|---|---|---|
| `claude` | Claude Code 自身(サブスクリプションのログイン) | 自分のプランで Anthropic のモデル。ゲートウェイは関与しないので、ルーティング・フォールバック・集計はなし |
| `llm-gateway launch claude` | ゲートウェイ(プロバイダーの認証情報) | 設定した全プロバイダー、内容分類、フォールバック、コスト集計 |
| 同じコマンドを `claude-cli` provider 入り設定で | **ローカルの `claude` バイナリ**(サブスクリプションのログイン) | ゲートウェイ*経由*で自分のプランを使う — 分類と集計込み、CLI が運べない分だけ差し引かれる |

3 つ目は「自分のサブスクリプションを使いたいが、ゲートウェイも使いたい」への
答えです。Claude Pro/Max のプランは Claude Code のための認証情報であって
API キーではないので、ゲートウェイはそれを上流に提示できません — ですが、
ログイン済みの公式クライアントを実行することはできます:

```json5
providers: {
  // baseUrl も apiKey も無し: CLI 自身が認証する。
  "claude-subscription": { api: "anthropic-messages", transport: "claude-cli" },
},
routes: {
  "role-sub": {
    description: "ローカルの Claude CLI を通して自分の Claude サブスクリプションで処理すべき依頼。生成専用で、呼び出し元のツールは渡されない。",
    model: { default: "claude-subscription/sonnet" },
  },
  "default": {
    description: "他の route に明確に当てはまらないリクエストの受け皿。",
    model: { default: "anthropic/*" },
  },
}
```

昔の launch 時 route 上書きはなくなりました。この route にある種の依頼を勝たせたい
なら、その依頼を本当に見分けられる `description` を与えてください。決めるのは
分類であって、クライアント側の model picker ではありません。

この route ができないのは、**あなたの**ツールを実行することです: `claude -p`
が持つのは Claude Code 自身のツールであり、リクエストに含まれるツールでは
ありません。ゲートウェイはそれらすべてを拒否する(`--allowedTools ""`)ので、
プロバイダー呼び出しがあなたのファイルに触れることはできません。つまり
これは agent ループではなく、生成を一段上流で行っているだけです。
`docs/ja/gotchas.md` に残りの制約があります(マルチターンのフラット化、
呼び出しごとの ~5 秒のプロセス起動、サンプリングパラメータの破棄)。

代わりに**フル API 忠実度でゲートウェイ経由の Claude モデル**に届きたいなら
— ツール、マルチターン、API 定義どおりのストリーミング込みで — API
アクセスを売っているプロバイダーから買うことになります:

- **OpenRouter** — `openrouter-anthropic/anthropic/*`。Anthropic のワイヤー
  プロトコルなので変換なし。
- **GitHub Copilot** — 公式の API を持つサブスクリプションなので、Copilot の
  プランはゲートウェイのトラフィックを実際に処理できます。`docs/ja/providers.md`
  を参照。モデルは Copilot の設定で有効化しておく必要があります。

`init` で Anthropic のキーを空のままにするのは問題なく、壊れ方も予測可能です:
参照は `${ANTHROPIC_API_KEY}` のまま残り、認証情報が解決できないターゲットは
リクエストを失敗させるのではなく**次のフォールバックに読み飛ばされます**
(`llm-gateway trace` に `key_unresolved` として出ます)。`init` が Anthropic +
OpenRouter 構成で書き出す設定なら、Anthropic 系のトラフィックは変数を設定する
まで OpenRouter に着地します。

## Claude Code を Anthropic 以外のプロバイダーへルーティングする

Claude Code は Anthropic Messages しか話しませんが、ゲートウェイは
`anthropic-messages` → `openai-chat` を変換するので、OpenAI 互換の
プロバイダーなら送り先として何でも使えます — Ollama(ローカル/クラウド)、
Groq、DeepSeek、Gemini、Mistral、Together、Sakana AI、PLaMo。

普通の route を、実のある `description` 付きで追加します(route 名に `:` や `/`
は使えないので、プロバイダー自身のモデル id は右辺にそのまま残せます):

```json5
providers: {
  "ollama-local": { baseUrl: "http://127.0.0.1:11434/v1", api: "openai-chat" },
},
routes: {
  "default": {
    description: "他の route に明確に当てはまらないリクエストの受け皿。",
    model: { default: "anthropic/*" },
  },
  "role-cheap": {
    description: "短い定型作業: 要約、整形、コミットメッセージ生成",
    model: { default: "ollama-local/qwen3:8b" },
  },
}
```

あとは Claude Code を普通に使うだけです。最後の user message が他の route
より `role-cheap` によく合うと分類されたとき、その route に送られます。
Claude の `/model` UI でこの route を**強制**することはできません。

このような route で諦めることになるものは README にまとまっています
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
| 400 mentioning betas / `context_management` | `CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`(切り分けのため一時的に) |
| settings の変更が反映されない | settings の `env` はシェルの export を上書きする — シェルではなくファイルを確認 |
| count_tokens のエラー | ゲートウェイは `/v1/messages/count_tokens` を転送する; `llm-gateway providers` を確認 |
| `openai-chat` の route でコンテキストサイズがおかしく見える | 想定どおり: そのプロトコルにはカウント用エンドポイントがないため、数値はローカルの推定値 |
| ローカルモデルが最初は何も答えず、途中から急に答え始めるように見える | `reasoning_content` が変換で破棄されているだけ; 転送されるのは答えの部分のみ |

## 元に戻す

`ANTHROPIC_BASE_URL` の行を削除して Claude Code を再起動。1 行だけです。
