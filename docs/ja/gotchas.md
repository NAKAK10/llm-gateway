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

## GitHub Copilot

- **認証情報はただの GitHub トークンで、`Authorization: Bearer` として使う。**
  Copilot 専用の API キーもトークン交換のステップもない —
  古い連携が使っていた `copilot_internal/v2/token` は不要で、素の HTTP
  クライアントに対しては `403` を返すだけ。探しに行かないこと。
- **`gh auth token` は*アクティブな*アカウントのトークンを返す。** 複数
  アカウントでログインしていると、それが黙って間違ったアカウントになる —
  ライセンスを持つのが別アカウントの場合、`403 unauthorized: not licensed to
  use Copilot` になる。`command:gh auth token --user <login>` で固定すること。
- **他のツールのモデルピッカーは entitlement の証拠にならない。** 例えば
  opencode はライブの `/models` 取得に失敗するとキャッシュ済みの models.dev
  カタログにフォールバックするので、そのアカウントで使えないモデルまで一覧に
  出続ける。entitlement は一覧ではなく、実際に生成させて確認すること。
- **403 は 2 種類あり、原因が違う。** `unauthorized: not licensed to use
  Copilot` は API 向けの Copilot entitlement が無い状態 — 実際には未払いや
  失効が最も多いので、まず支払い状況を確認する。`unauthorized: not
  authorized to use this Copilot feature` は entitlement はあるがこの
  トークンまたは機能が対象外の状態 — サブスクリプションが組織由来なら、組織側の
  Copilot ポリシーとシート割り当てを確認する。
- **一覧に載っているモデルが使えるモデルとは限らない。** `/models` にはプランで
  触れないモデルも含まれ、それらは `400 model_not_supported`(アカウントに
  無いエンドポイントには `no_available_model_endpoints`)を返す。route に
  入れる前に `policy.state` でフィルタし、GitHub Copilot の設定でプレミアム
  モデルを有効化しておくこと。
- **Copilot は Claude モデル向けに `/v1/messages` も公開している** — これなら
  変換を完全にスキップできるはずだが、`Authorization: Bearer` を要求する一方、
  このゲートウェイの `anthropic-messages` プロバイダーは `x-api-key` で
  認証する。プロバイダーが認証ヘッダーを選べるようになるまで、Copilot は
  `openai-chat` プロバイダーとしてのみ使える。
- **`x-initiator` / `Openai-Intent` は意図的に送らない。** Copilot はこれらで
  トラフィックを分類しており、正しい値は個々のリクエスト(人間のターンか
  ツールループか)に依存する — ゲートウェイ全体で一定の値を送れば半分は
  誤りになる。

## Agent CLI トランスポート(`claude-cli`)

- **子プロセスがゲートウェイを呼び返せてはならない。** `~/.claude/settings.json`
  の `env` ブロックは `ANTHROPIC_BASE_URL` を設定でき、継承された環境変数
  も同様にありうる — どちらか一方でもプロバイダー呼び出しが無限ループに
  なる。独立した 2 つのガードがある: 子プロセスは空のスクラッチ
  ディレクトリで `--setting-sources project` を付けて実行され(ユーザー
  設定が一切読み込まれない)、かつそれらの環境変数は子プロセスの環境から
  取り除かれる。どちらか一方を「簡略化」で外してはいけない。
- **呼び出し元のツールは引き継げず、子プロセス側のツールは拒否しなければ
  ならない。** `claude -p` は Claude Code 自身のツールを実行する —
  `--allowedTools ""` がなければ、プロバイダー呼び出しが誰も頼んでいない
  ファイル編集をしてしまう可能性がある。検証済み: 空の allowlist でも
  モデルはツールを*試みる*ことがあり、その試みは権限プロンプトでハング
  することなく拒否される。
- **`kill_on_drop` は省略できない。** クライアントが切断するとレスポンス
  ストリームは失われる — これがないと `claude` プロセスは誰にも見られず、
  誰の得にもならないまま生成を続ける。
- **`assistant` イベントの `usage` は実行途中のスナップショットである。**
  実測: 5 トークンの回答でもそこでは `output_tokens: 1` と報告され、
  `result` イベントでは 5 になる。非ストリーミング経路はコスト集計が
  そのボディを読むため `result` の数値を採用する。
- **`-p` には `--output-format stream-json` と併せて `--verbose` が
  必須。** 出力を本物の Anthropic ストリームイベントに変えるのは
  `--include-partial-messages` であり、これが無いと 1 個の完全な
  メッセージが返るだけでストリーミングにならない。
- **プロセス起動がレイテンシの大半を占める**(ここでは呼び出しごとに
  ~5 秒)。この transport はそれが許容できる route 向けであり、HTTP
  プロバイダーの drop-in 代替ではない。
- **リクエストは API の残高ではなくサブスクリプションの制限を消費する。**
  混雑した route はプランのクォータを使い切ることがあり、その失敗は
  CLI 自身のメッセージ(「usage limit reached」)を伴う `error` フレーム
  として届く。

- **`count_tokens` でプロセスを起動してはいけない。** CLI でトークン数を数える
  手段は「実際に 1 回生成させる」以外に無いので、`claude-cli` ターゲットは
  ローカル推定の経路を通す(トレースログでは `estimated_locally`)。ここを
  トランスポートに繋いでしまうと、数えるためだけに回答 1 回分を消費する。
## 設定 / セキュリティ

- **`command:` の秘密情報参照はリクエスト試行のたびに実行される。** これが
  ローテーションするトークン(`gh auth token`)を再起動なしで扱える理由だが、
  リクエストごとのコストでもある — 呼び出しのたびにネットワークへ出る
  ヘルパーは、ここではなくキャッシュする層の裏に置くべき。`${VAR}` では
  代替できない: `serve` プロセスの環境変数は起動時に固定され、外部から
  更新する手段がないため。

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
