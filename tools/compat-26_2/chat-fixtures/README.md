# 26.2 chat fixture set

このディレクトリは、26.2 secure-chat の次段 stateful 実装へ渡す独立 fixture だけを
置く。runtime harness や大規模 framework ではない。正式 code はこの作業で変更して
いない。

## Provenance

- canonical field order と `LastSeenMessages.Update` の offset/20-bit ack の根拠は、
  repository 外の公式 26.2 server JAR の static bytecode inspection。JAR は実行せず、
  repository にコピーしない。
- `canonical-empty-last-seen.json` の field 値、canonical hash、RSA public DER/signature
  はこの repository の test-only known-answer。signature は OpenSSL 3.2.2 で生成した
  2048-bit test key の結果で、Mojang/vanilla fixture ではない。private key は保存しない。
- `chain-state.json`、`last-seen-ack.json`、`policy.json` は state-machine の入力と
  expected contract。signature label は cryptographic bytes ではなく、次 owner が
  existing stateless helper/ack preview に結合するための名前である。
- `signed-command.json` は wire/parser と policy の分離を示す。argument signature の
  canonical signed bytes は未確認のため、`deferred` を pass/fail に置き換えない。

## File contract

各 JSON は `schema_version=1`、`evidence` を持ち、state/policy 系は `cases` を持つ。
canonical known-answer は単一 `input`/`expected` と `negative_mutations` を持つ。case の
`expected.decision` は `accept`、`reject`、`defer` のいずれかである。`reject_reason`
は vanilla の表示文言ではなく、owner 間で安定させる fixture ID である。

`signed-command-independent.json` は signed-command argument 用の独立 known-answer で
ある。N=1 の plain/quoted/greedy/Unicode raw substring と N=2 の quoted+greedy を含み、
各 argument の canonical bytes、SHA-256、256-byte RSA/SHA-256 signature、test-only
SPKI public DER を固定する。canonical body は official chat body layout を使うが、鍵と
signature は OpenSSL 3.2.2 で生成した自作 fixture であり、Mojang/vanilla capture では
ない。`last-seen-ack.json` の invalid ACK は署名 fixture とは別の reject contract である。

## Secret and rights policy

ここに公式 JAR、Mojang public/private key、session private key、実ユーザー token、
packet capture、world/log artifact を置かない。public DER/signature は test-only で
あり、秘密鍵から再生成する test は想定しない。公式 JAR の利用は静的確認と hash 記録
に限定し、配布物を複製しない。
