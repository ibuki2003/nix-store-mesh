# nix-store-mesh

信用する端末間でNix storeを共有し、Nixには単一のローカルbinary cacheとして提供します。
peer間の認証・暗号化とアドレス管理にはTailscaleを使用します。

## 起動

各端末でNixとTailscaleを利用できる状態にし、`config.example.toml`を元に設定を用意してください。
`peer_listen`には自端末のTailscale IP、`peers`には取得先端末のTailscale IPまたはMagicDNS名を指定します。
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

Tailscaleのアクセス制御で、peerポートへ接続できる端末を限定してください。
`peers`は取得先の設定であり、自端末のstoreの公開先を制限する設定ではありません。
Tailscale経由での通信に限定しますが、LAN内だけを通ることは保証しません。

## コードの入口

- `src/config.rs`：設定
- `src/remote/`：peer接続とローカルcache
- `src/store.rs`：自端末のstore公開
