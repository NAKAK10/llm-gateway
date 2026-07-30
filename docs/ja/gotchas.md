# ハマりどころ

[English](../gotchas.md) | 日本語

既知の罠の一覧です。ここに載っていないものを踏んだら、追記してください。

## ストリーミング / プロキシ

- **アップストリームのレスポンスから `content-length`、`transfer-encoding`、
  `content-encoding` をコピーしてはいけない。** axum がボディを再フレーム化
  するため、古い値が残るとクライアントがハングしたり、平文を展開しようと
  したりする。(`server/passthrough.rs` が除去している。)
- **reqwest の gzip/brotli フィーチャーを有効にしてはいけない。** 経路の
  途中にデコーダーが入るとチャンクが再バッファリングされ、SSE のトークン
  ストリーミングが長い停止に化ける。代わりに `accept-encoding: identity`
  を送っている。
- **タイムアウトはリクエスト全体ではなく最初のバイトに掛ける。**
  `reqwest::timeout()` は長いが正常な生成をストリーム途中で殺してしまう。
  `connect_timeout` + ヘッダー期限(`FIRST_BYTE_TIMEOUT`)が正しい組み合わせ。
- **ストリーム開始後のフォールバックは不可能。** 200 と最初のチャンクは
  既に送信済み。これは物理法則であって未実装機能ではない。フォールバックの
  ユーザー向け説明には必ず明記すること。
- **クライアントが切断してもアップストリームのトークンは消費される。**
  usage の記録はハンドラーの正常系ではなく、ストリームの `Drop` から行う
  こと(`usage/tee.rs`)。
- **ストリーミングの OpenAI-chat リクエストには `stream_options.include_usage`
  を注入しなければならない。** さもないと usage が永遠に静かにゼロになる —
  そしてそれは「正常に動いている」ように見える。これで壊れるプロバイダーには
  `injectUsage: false` を設定する。

## launch / クライアント

- **Claude Code: `settings.json` の `env` はシェル環境より強い。** ユーザーが
  そこに `ANTHROPIC_BASE_URL` を書いた瞬間、`launch claude` は黙って
  リダイレクトしなくなる。検知して警告する
  (`launch/claude.rs::detect_conflicts`)。`--isolate`
  (`--setting-sources project`)は最終手段であってデフォルトではない。
- **Codex: 環境変数でアップストリームをリダイレクトする方法はない。**
  `OPENAI_BASE_URL` は存在しない。`-c model_providers.…` だけが機能し、
  値は TOML としてパースされる → 文字列には二重引用符の埋め込みが必要。
- **Codex: `--ignore-user-config` は `codex exec` にしかない。** TUI 実行は
  隔離できない。この非対称性は隠さず警告している。
- **Codex: `disable_response_storage=true` は常に付ける。** OpenRouter の
  `/v1/responses` はステートレスで、`previous_response_id` が非 null だと
  400 を返す — これがないと OpenRouter へのフォールバックはどの会話でも
  2 ターン目で死ぬ。
- **opencode: `models` のキーは `GET /v1/models` の id と完全一致でなければ
  ならない。** 一致しないと何も表示せず、何も言わない。`launch opencode`
  は起動前に稼働中のゲートウェイに対して検証する。
- **opencode: `OPENCODE_CONFIG` はプロジェクト設定に負ける。** 確実に勝つのは
  `OPENCODE_CONFIG_CONTENT` だけで、その中では `{env:VAR}`/`{file:…}` が
  展開**されない**(anomalyco/opencode#13219)— キーがリテラルで埋め込まれる。
  この環境変数がリダクション対象リストに入っているのはそのため。
- **OpenClaw: プロバイダーの `models` は許可リスト。** リストにないルート名は
  OpenClaw にとって存在しないのと同じで、エラーメッセージも役に立たない。
  ルートを追加したら許可リストも更新すること。
- **OpenClaw: フォールバックの二重化に注意。** OpenClaw は独自のモデル
  フォールバックチェーンを持つ。モデルレベルのフォールバックはゲートウェイに
  任せ、OpenClaw 側の `fallbacks` は「ゲートウェイが落ちた → 旧直結ルート」
  という単一の脱出ハッチにとどめる。さもないとすべての失敗があらゆる場所で
  二重にリトライされる。
- **OpenClaw: cron 実行にはシェル環境がない。** `${VAR}` のキー参照は
  ターミナルでは解決できても 09:01 に 401 になる。キーはデーモン自身の
  起動環境に入れること。
- **`localhost` は `::1` に解決されることがある。** すべての設定とドキュメントは
  `127.0.0.1` を使う。

## クロスプロトコル変換

方向は一つだけ: `anthropic-messages` 受信 → `openai-chat` 送信
(`src/translate/`)。走るのはクライアントとプロバイダーのプロトコルが
異なる場合*だけ*で、同一プロトコル同士のトラフィックはこの経路に一切入らない。

- **変換されたルートではバイト単位無加工の保証が成り立たない。** ユーザー
  向けの説明には必ずそう書くこと。「出力がおかしい」という報告を調べる前に
  `llm-gateway trace` で `xlat=anthropic-messages->openai-chat` を確認する。
- **静かに破棄されるもの(変換先プロトコルに置き場がないため):**
  プロンプトキャッシュ(`cache_control`。`cache_creation_input_tokens` は
  常に 0)、`thinking` ブロックと `thinking` リクエスト設定、citation、
  `document`/`search_result` コンテンツブロック、`top_k`、Anthropic の
  サーバーサイドツール(`web_search_*`、`bash_*`、`text_editor_*` —
  Anthropic 自身のインフラ内で実行されるものなので、どのみち他の
  プロバイダーには実行しようがない)。
- **`reasoning_content` / `reasoning` の delta は変換されず破棄される。**
  本物の Anthropic `thinking` ブロックは Anthropic だけが発行できる
  `signature` を持つ — reasoning を普通のテキストとして転送すれば、
  それが答えであるかのように見えてしまう。そのため reasoning の長い
  ローカルモデルは、答え始めるまで黙っているように見える。`max_tokens` が
  小さいと**本文がまったく無いまま返る**ことすらある(実測: `qwen3.5:4b`
  に `max_tokens: 64` — 64 トークンすべてを reasoning に使い、`content` は
  空文字列だった)。ゲートウェイのバグではなく、上流がそう返している。
- **`finish_reason: "stop"` が `tool_calls` と同時に来ることは珍しくない**
  (Ollama や複数の OpenAI 互換サーバー)。それでも Anthropic 側は
  `stop_reason: "tool_use"` を報告しなければならない — さもないとクライアントは
  渡されたツール呼び出しを一切実行しない。ストリーミング / 非ストリーミング
  どちらの変換器でも同じ規則。
- **`function.arguments` は OpenAI 側では JSON *文字列*、Anthropic 側では
  オブジェクト。** ストリーム中はフラグメントをそのまま
  `input_json_delta.partial_json` として転送しなければならない — 断片単体は
  有効な JSON ではなく、再シリアライズすると呼び出しが壊れる。
- **Ollama はストリーミングのツール呼び出しで `index` と `id` を省略する。**
  `index` だけでブロックをキーイングすると 2 つの呼び出しが 1 つに
  合体してしまう。id は合成するしかない。
- **終端イベントはどの経路でも必ず送出しなければならない。** `[DONE]`、
  `finish_reason`、ストリーム途中の `{"error":…}` フレーム、あるいは
  アップストリームがただ止まる場合 — どの場合でもクライアントは
  `content_block_stop` + `message_delta` + `message_stop` を受け取らなければ
  永遠に待ち続ける。
- **`/v1/messages/count_tokens` は転送できない**(`openai-chat` に該当する
  エンドポイントがない)ためローカルの推定値で応答する。カウントではなく
  推定値であることは、トレースログの `result: "estimated_locally"` で
  見分けられる。
- **使用量集計には影響しない** — `usage/tee.rs` は変換レイヤーより下の
  アップストリームのバイト列を観測している。この観測ポイントを変換レイヤーの
  上に移してしまうと、変換されたリクエストはすべて*変換器側*の数値を
  報告し始める。

## 設定 / セキュリティ

- `config.json` はリテラルのキーを保持しうる → 作成時に `0600`、権限ドリフトは
  警告、`config show` と `launch --print` ではマスク、`config gitignore` で
  テンプレート出力。
- `server.apiKey` なしのループバック以外へのバインドは起動時に拒否:
  ポートの向こうのすべてのプロバイダー認証情報を 1 つのキーで守る。
- `server.host`/`server.port`/`server.apiKey` はホットリロード**されない**
  (リスナーとその識別情報はバインド時に固定)。それ以外はすべてリロードされる。
- リロード失敗時は旧設定が生き続ける — 仕様。編集が反映されたと思い込む前に
  stderr を確認すること。ログ行に何が変わったか書いてある。
- ルート名に `:` と `/` は使えない(opencode のモデルキー、Codex の TOML
  キー、URL パスを壊す)。モデルの*値*には両方使える — パースは最初の `/`
  でのみ分割するため、`openrouter/anthropic/claude-…` も
  `ollama-cloud/glm-5.2:cloud` も動く。
- `--debug` はプロンプト本文を書き込む(200 文字で切り詰め; `--debug-full`
  は切り詰めない)。業務の会話が平文で `logs/` に残る。

## Rust まわり

- axum 0.7+ で `StreamBody` は削除された; `Body::from_stream` を使う。
- macOS の `notify`(FSEvents)は自分の所有でないファイルの扱いに難がある →
  ウォッチャーは親ディレクトリを監視してファイル名でフィルタし、エディタの
  アトミック保存対策に 300 ms のデバウンスを入れている。
- `dirs`/`directories` は macOS で `~/Library/Application Support` を返し、
  XDG を尊重しない。設定が `~/.config` に置かれるのは
  `etcetera::choose_base_strategy()` を使っているため。
