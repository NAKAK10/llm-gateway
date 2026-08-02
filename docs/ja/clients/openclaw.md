# OpenClaw — 手動セットアップ(`launch` 非対応)

[English](../../clients/openclaw.md) | 日本語

OpenClaw は独自のスケジューラーを持つデーモンとして、多くの場合
ゲートウェイとは**別のマシン**で動きます。`launch` が起動すべきプロセスが
存在しないため、手動かつ段階的な移行になります — そして 09:01 の cron に
日次コンテンツパイプラインがぶら下がっている以上、段階的であることは
任意ではありません。

## 0. まず到達性

`http://127.0.0.1:4000` は別ホストからは存在しません。OpenClaw に触る前に
到達手段を決めます:

- **推奨: Tailscale。** ゲートウェイを tailnet の IP にバインドし、
  `server.apiKey` を設定する(ループバック以外へのバインドはキーなしでは
  拒否される)。
- LAN 上の素の `0.0.0.0` は**絶対に不可**: キーが 1 つ漏れれば、設定内の
  すべてのプロバイダー認証情報が、ポートを見つけた誰にでも使われる。

先に OpenClaw ホストから確認:

```sh
curl -s -H "Authorization: Bearer $KEY" http://<tailnet-ip>:4000/v1/models
```

## auto-route と、OpenClaw ではなぜ切りたくなるか

このゲートウェイは既定で**クライアントが送ってきたモデル名を無視**し、
受信したリクエストの内容を各 route の `description` に対して分類することで
route を決めます(README の
[コンテンツ分類ルーティング](../../../README.ja.md#コンテンツ分類ルーティング)
参照)。これは Claude Code のように model 文字列がほぼ飾りであるハーネス
には合っています。しかし OpenClaw のように、内部呼び出しごと(タイトル
生成、エージェントごとの判定呼び出しなど)に明示的なモデル名を指定する
エージェントループを持つクライアントには、この既定は合いません —
分類がそれぞれの選択を勝手に上書きしてしまいます。

このオプトアウトはすでに存在しており、ゲートウェイ側の新機能は不要です:
リクエストに `x-gw-auto-route: 0` を付けると、ゲートウェイは分類を
スキップし、クライアントが送ってきたモデル名をそのまま route 名として
解決します(`src/server/proxy.rs` の `auto_route_requested` 参照)。
`llm-gateway launch` はこれをセッションごとに対話的に尋ねますが — 冒頭で
書いたとおり OpenClaw は `launch` に非対応(多くの場合別マシンで動く
常駐デーモン)なので、尋ねるタイミング自体がありません。ヘッダーは
OpenClaw 側で固定し、送信するすべてのリクエストに付くようにする必要が
あります。

OpenClaw が provider ごとに固定の追加ヘッダーを設定できるかどうか、
できるとしてどう設定するかは、このドキュメントでは断定できません —
OpenClaw 自身の設定スキーマに依存し、ここでは未確認です。上の `gateway`
provider ブロックに静的ヘッダーを付ける方法を OpenClaw 側のドキュメントや
設定リファレンスで確認し、次の値を設定してください:

```
x-gw-auto-route: 0
```

**これはモデル名の解決方法も変えます。** auto-route を無効にすると、
`src/route.rs` の `find_route` は送られてきたモデル名を route のキーとして
プレーンな完全一致でルックアップします — prefix マッチもワイルドカードも
あいまい一致もありません(opencode の `overrideProviders` について
[`docs/ja/clients/opencode.md`](opencode.md) に書いたのと同じ挙動です)。
そのため、OpenClaw が送るよう設定されたすべてのモデル名 — 下の手順 1 の
`models` リスト — は `config.json` 内の route の**リテラルな名前と一致**
していなければなりません。一致する route が無いモデル名は、分類にフォール
スルーせず 404 になります。

ヘッダーが実際にゲートウェイへ届くかどうかを、OpenClaw の設定とは切り離して
確認できる curl の例です:

```sh
curl -s -H "Authorization: Bearer $KEY" \
     -H "x-gw-auto-route: 0" \
     -H "Content-Type: application/json" \
     -d '{"model":"role-researcher","messages":[{"role":"user","content":"ping"}]}' \
     http://<tailnet-ip>:4000/v1/chat/completions
```

`x-gw-auto-route: 0` を付けると `role-researcher` は完全一致の route 名として
解決されます(分類は一切走りません)ので、これが成功するのは
`config.json` に実在する route `role-researcher` がある場合だけです。
ヘッダーを外す(または `1` にする)と、同じ呼び出しは `model` フィールドを
一切見ずに分類経由になります。

## 1. プロバイダーを追加する(動いているものには触れない)

OpenClaw ホストの `openclaw.json` に:

```json5
{
  models: {
    providers: {
      gateway: {
        name: "LLM Gateway",
        baseUrl: "http://<tailnet-ip>:4000/v1",
        apiKey: "<server.apiKey の値>",   // cron にはシェル環境がない — ${VAR}
                                          // 参照はターミナルでは解決できても
                                          // 09:01 に 401 になる。リテラルにするか、
                                          // 変数をデーモン自身の環境に入れる。
        api: "openai-completions",
        // ★ 見せたいルートはすべて列挙しなければならない。載っていない名前は
        //   OpenClaw にとって存在しないのと同じ。
        models: ["role-manager", "role-researcher", "role-writer",
                 "role-reviewer", "role-publisher"],
      },
    },
  },
}
```

まだどのエージェントもここを向いていないので、稼働中のシステムは無変更です。
OpenClaw のモデル一覧にモデルが見えることを確認します。

## 2. リスクの低いエージェントを 1 つ移行する

最初は **researcher** を切り替えます(リサーチ工程の失敗は実行品質を
落とすだけで、実行そのものは殺さない):

```json5
agents: {
  entries: {
    "ekkohappy-researcher": {
      model: {
        primary: "gateway/role-researcher",
        // ★ 旧・直結ルートを最終手段のフォールバックとして残す。
        //   ゲートウェイのマシンが落ちていても、明日の記事は出る。
        fallbacks: ["ollama-cloud/deepseek-v4-flash"],
      },
    },
  },
},
```

モデルレベルのフォールバックはゲートウェイに任せます。ここでの OpenClaw の
`fallbacks` は「ゲートウェイ到達不能 → 旧直結パス」という単一の脱出ハッチ
です。2 つのフォールバック機構が同じ失敗をそれぞれリトライすると、
レイテンシもコストも倍になります。

本番の 1 サイクル(朝の cron + そのウォッチドッグレポート)を丸ごと見届け、
その実行が `llm-gateway stats --by client` に現れることを確認します。

## 3. 残りは 1 日 1 つずつ

writer → reviewer → publisher → **controller は最後**(パイプラインを
駆動しているため、壊れると他が何も始まらない)。すでに元を取ったルール:

- 切り替えは、ウォッチドッグレポートを人間が見る日に行う。金曜の夜や
  休日前は避ける。
- 各切り替えの後、**同日中に手動でフル実行**をトリガーする。翌朝の cron を
  最初のテストにしてはならない。

## 元に戻す

- 最速: ゲートウェイを停止する — 各エージェントの `fallbacks` が勝手に
  旧直結ルートへ落とす。
- クリーン: `primary` を旧来の `ollama-cloud/…` の値に戻す(ステップ 2 で
  ファイルに残しておいたのはこのため)。
- 大惨事(エージェント登録の消失 — 実際に起きたことがある):
  `openclaw agents add <name> --workspace … --model <old-model>` で各
  エージェントを再登録し、cron を作り直す。ステップ 1 より前の
  `openclaw.json` のコピーを保管しておくこと。
