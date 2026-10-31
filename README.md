# nix-store-mesh

信用する端末間でNix storeを共有し、Nixには単一のローカルbinary cacheとして提供します。
peer間の認証・暗号化とアドレス管理にはTailscaleを使用します。

## 起動

各端末でNixとTailscaleを利用できる状態にし、`config.example.toml`を元に設定を用意してください。
`listen`には待受アドレスを指定します。`0.0.0.0`なら全IPv4アドレスで待ち受けます。
`hosts`には自端末を含めて、OS hostnameをkey、Tailscale IPまたはMagicDNS名を値として列挙できます。
OS hostnameと完全一致するkeyは取得先から除外します。自端末の項目がなくても起動できます。
待受・接続先とも、ポートを省略すると`9722`を使用します。変更する場合は`host:port`（IPv6は`[address]:port`）で指定します。

```sh
cargo run --release -- ./config.toml
```

パスを省略すると`$XDG_CONFIG_HOME/nix-store-mesh/config.toml`を読みます。
`XDG_CONFIG_HOME`が未設定・空・相対パスの場合は、`~/.config/nix-store-mesh/config.toml`を使用します。

Nixの管理者設定（通常は`/etc/nix/nix.conf`）に追加します。

```ini
extra-substituters = http://127.0.0.1:9722?trusted=true
```

## 信頼の範囲

`trusted=true`により、mesh経由のstore pathにはNixの署名を要求しません。
取得先peerは、任意の実行ファイルを供給できる端末として信用してください。
ローカルのcache入口も、その信用を与えるサービスとして管理する必要があります。

loopbackからの接続にはローカルcache、それ以外には自端末のstore・ビルドログを公開します。
`hosts`は取得先の設定であり、公開先を制限する設定ではありません。
公開先を限定したい場合はfirewall等で制御してください。公開によりデータの閲覧と転送リソースの消費を許可することになります。
取得先への接続はTailscaleアドレスに限定しますが、LAN内だけを通ることは保証しません。

## コードの入口

- `src/config.rs`：設定
- `src/remote/`：peer接続とローカルcache
- `src/store.rs`：自端末のstore公開
