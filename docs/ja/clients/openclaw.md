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
